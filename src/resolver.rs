//! Resolve the declared graph before changing any project files.
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use petgraph::algo::{astar, toposort};
use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::visit::Dfs;
use snafu::{ResultExt, whatever};

use crate::Result;
use crate::context::Context;
use crate::lockfile::{LockedPackage, Lockfile};
use crate::manifest::{Dependency, Manifest};
use crate::package::{IMPORT_VERSION, Package};
use crate::sources::{
    LocatedPackage, PackageIdentity, PackagePin, RefreshPolicy, SourceId, SourceMap,
};

struct PackageNode {
    id: String,
    location_id: String,
    package: Package,
    namespace: String,
    pin: Option<PackagePin>,
}

impl PackageNode {
    fn new(package: Package, identity: PackageIdentity, namespace: &str) -> Self {
        Self {
            id: identity.package_id(namespace),
            location_id: identity.location_id,
            package,
            namespace: namespace.to_owned(),
            pin: identity.pin,
        }
    }

    fn label(&self) -> String {
        format!(
            "@{}/{}:{}",
            self.namespace, self.package.name, self.package.version
        )
    }
}

struct ImportRequirement {
    root: NodeIndex,
    node: NodeIndex,
    import: ResolvedImport,
}

pub struct ResolvedImport {
    pub package: Package,
    pub namespace: String,
    pub version: String,
    pub roots: BTreeSet<String>,
}

pub struct Resolution {
    pub lock: Lockfile,
    pub imports: Vec<ResolvedImport>,
}

pub struct Resolver<'context> {
    sources: SourceMap<'context>,
    graph: DiGraph<PackageNode, ()>,
    package_nodes: BTreeMap<String, NodeIndex>,
    direct_dependencies: BTreeMap<String, NodeIndex>,
}

impl<'context> Resolver<'context> {
    pub fn new(
        context: &'context Context,
        previous_lock: Lockfile,
        refresh: RefreshPolicy,
    ) -> Self {
        Self {
            sources: SourceMap::new(context, previous_lock, refresh),
            graph: DiGraph::new(),
            package_nodes: BTreeMap::new(),
            direct_dependencies: BTreeMap::new(),
        }
    }

    pub fn infer_name(
        &mut self,
        dependency: &Dependency,
        base: &Path,
        expected: Option<&str>,
    ) -> Result<String> {
        let located = self.sources.locate(dependency, base, None, expected)?;
        Ok(located.package.name)
    }

    pub fn resolve(mut self, manifest: &Manifest, base: &Path) -> Result<Resolution> {
        for (name, dependency) in &manifest.dependencies {
            let node = self
                .visit_dependency(dependency, base, None, name, 0)
                .with_whatever_context(|_| format!("while resolving direct dependency `{name}`"))?;
            self.direct_dependencies.insert(name.clone(), node);
        }
        self.check_cycles()?;
        let imports = self.resolve_imports()?;
        let lock = self.build_lockfile();
        Ok(Resolution { lock, imports })
    }

    fn visit_dependency(
        &mut self,
        dependency: &Dependency,
        base: &Path,
        parent: Option<&SourceId>,
        expected: &str,
        depth: usize,
    ) -> Result<NodeIndex> {
        if depth > 256 {
            whatever!("dependency graph exceeds 256 nested packages");
        }
        let located = self
            .sources
            .locate(dependency, base, parent, Some(expected))?;
        let identity = self.sources.identity(&located)?;
        let LocatedPackage { package, source } = located;
        let package_root = package.root.clone();
        let package_node = PackageNode::new(package, identity, &dependency.namespace);
        if let Some(node) = self.package_nodes.get(&package_node.id) {
            return Ok(*node);
        }
        let id = package_node.id.clone();
        let node = self.graph.add_node(package_node);
        self.package_nodes.insert(id, node);
        let manifest = self.sources.manifest(&source, &package_root)?;
        for (name, dependency) in &manifest.manifest.dependencies {
            let child = self
                .visit_dependency(dependency, &package_root, Some(&source), name, depth + 1)
                .with_whatever_context(|_| {
                    format!("{} depends on `{name}`", self.graph[node].label())
                })?;
            self.graph.add_edge(node, child, ());
        }
        Ok(node)
    }

    fn check_cycles(&self) -> Result<()> {
        if let Err(cycle) = toposort(&self.graph, None) {
            let start = cycle.node_id();
            for neighbor in self.graph.neighbors(start) {
                if let Some((_, path)) =
                    astar(&self.graph, neighbor, |node| node == start, |_| 1, |_| 0)
                {
                    let chain = std::iter::once(start)
                        .chain(path)
                        .map(|node| self.graph[node].label())
                        .collect::<Vec<_>>()
                        .join(" -> ");
                    whatever!("dependency cycle: {chain}");
                }
            }
            whatever!("dependency cycle involving {}", self.graph[start].label());
        }
        Ok(())
    }

    fn dependency_chain(&self, root: NodeIndex, target: NodeIndex) -> String {
        astar(&self.graph, root, |node| node == target, |_| 1, |_| 0)
            .map(|(_, path)| {
                path.into_iter()
                    .map(|node| self.graph[node].label())
                    .collect::<Vec<_>>()
                    .join(" -> ")
            })
            .unwrap_or_else(|| self.graph[target].label())
    }

    fn resolve_imports(&self) -> Result<Vec<ResolvedImport>> {
        let mut requirements: BTreeMap<PathBuf, ImportRequirement> = BTreeMap::new();
        for (name, &root) in &self.direct_dependencies {
            let mut traversal = Dfs::new(&self.graph, root);
            while let Some(index) = traversal.next(&self.graph) {
                let node = &self.graph[index];
                // A package reached both directly and through another root needs
                // both import paths.
                let version = if index == root {
                    IMPORT_VERSION
                } else {
                    node.package.version.as_str()
                };
                let path = PathBuf::from(&node.namespace)
                    .join(&node.package.name)
                    .join(version);
                if let Some(requirement) = requirements.get_mut(&path) {
                    if self.graph[requirement.node].location_id != node.location_id {
                        whatever!(
                            "conflicting sources for @{}/{}:{}:\n  {}\n  {}\nPackages with the same namespace, name, and import version must use the same source",
                            node.namespace,
                            node.package.name,
                            version,
                            self.dependency_chain(requirement.root, requirement.node),
                            self.dependency_chain(root, index)
                        );
                    }
                    requirement.import.roots.insert(name.clone());
                } else {
                    requirements.insert(
                        path,
                        ImportRequirement {
                            root,
                            node: index,
                            import: ResolvedImport {
                                package: node.package.clone(),
                                namespace: node.namespace.clone(),
                                version: version.to_owned(),
                                roots: BTreeSet::from([name.clone()]),
                            },
                        },
                    );
                }
            }
        }
        Ok(requirements
            .into_values()
            .map(|requirement| requirement.import)
            .collect())
    }

    fn build_lockfile(&self) -> Lockfile {
        let mut lock = Lockfile::default();
        for index in self.graph.node_indices() {
            let node = &self.graph[index];
            let Some(pin) = &node.pin else {
                continue;
            };
            lock.sources
                .insert(pin.source_id.clone(), pin.source.clone());
            let mut dependencies: Vec<_> = self
                .graph
                .neighbors(index)
                .map(|child| self.graph[child].id.clone())
                .collect();
            dependencies.sort();
            dependencies.dedup();
            lock.packages.insert(
                node.id.clone(),
                LockedPackage {
                    source: pin.source_id.clone(),
                    directory: pin.directory.clone(),
                    name: node.package.name.clone(),
                    version: node.package.version.clone(),
                    namespace: node.namespace.clone(),
                    dependencies,
                },
            );
        }
        for (name, &root) in &self.direct_dependencies {
            let entrypoints = self.lockfile_entrypoints(root);
            if !entrypoints.is_empty() {
                lock.roots.insert(name.clone(), entrypoints);
            }
        }
        lock
    }

    fn lockfile_entrypoints(&self, root: NodeIndex) -> Vec<String> {
        let mut pending = vec![root];
        let mut visited = BTreeSet::new();
        let mut entrypoints = BTreeSet::new();
        while let Some(index) = pending.pop() {
            if !visited.insert(index.index()) {
                continue;
            }
            let node = &self.graph[index];
            if node.pin.is_some() {
                entrypoints.insert(node.id.clone());
            } else {
                pending.extend(self.graph.neighbors(index));
            }
        }
        entrypoints.into_iter().collect()
    }
}
