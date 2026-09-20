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
use crate::package::Package;
use crate::project::ManagedLink;
use crate::sources::{
    LocatedPackage, PackageIdentity, PackagePin, RefreshPolicy, SourceId, SourceMap,
};

pub use crate::sources::normalize_git_location;

struct PackageNode {
    id: String,
    location_id: String,
    package: Package,
    namespace: String,
    source: SourceId,
    pin: Option<PackagePin>,
}

impl PackageNode {
    fn new(located: LocatedPackage, identity: PackageIdentity, namespace: String) -> Self {
        Self {
            id: identity.package_id(&namespace),
            location_id: identity.location_id,
            package: located.package,
            namespace,
            source: located.source,
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

struct LinkRequirement {
    location_id: String,
    dependency_chain: String,
    link: ManagedLink,
}

pub struct Resolution {
    pub lock: Lockfile,
    pub links: Vec<ManagedLink>,
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
        let links = self.managed_links()?;
        let lock = self.build_lockfile();
        Ok(Resolution { lock, links })
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
        let package_root = located.package.root.clone();
        let package_node = PackageNode::new(located, identity, dependency.namespace.clone());
        if let Some(node) = self.package_nodes.get(&package_node.id) {
            return Ok(*node);
        }
        let id = package_node.id.clone();
        let source = package_node.source.clone();
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

    fn managed_links(&self) -> Result<Vec<ManagedLink>> {
        let mut requirements: BTreeMap<PathBuf, LinkRequirement> = BTreeMap::new();
        for (name, &root) in &self.direct_dependencies {
            let mut traversal = Dfs::new(&self.graph, root);
            while let Some(index) = traversal.next(&self.graph) {
                let node = &self.graph[index];
                let path = PathBuf::from(&node.namespace)
                    .join(&node.package.name)
                    .join(&node.package.version);
                let chain = self.dependency_chain(root, index);
                if let Some(requirement) = requirements.get_mut(&path) {
                    if requirement.location_id != node.location_id {
                        whatever!(
                            "conflicting sources for {}:\n  {}\n  {}\nBoth require the same namespace, name, and version",
                            node.label(),
                            requirement.dependency_chain,
                            chain
                        );
                    }
                    requirement.link.roots.insert(name.clone());
                } else {
                    let link = ManagedLink {
                        path: path.clone(),
                        target: node.package.root.clone(),
                        roots: BTreeSet::from([name.clone()]),
                    };
                    requirements.insert(
                        path,
                        LinkRequirement {
                            location_id: node.location_id.clone(),
                            dependency_chain: chain,
                            link,
                        },
                    );
                }
            }
        }
        Ok(requirements
            .into_values()
            .map(|requirement| requirement.link)
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
