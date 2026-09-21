# typm

`typm` is an experimental package manager for `typst` that mimic `cargo`.

## Install

### Cargo

```shell
cargo install typm
```

### Build from Source

```shell
git submodule update --init -- vendor/typst
cargo build --release
```

The upstream CLI definition at `src/typst_args.rs` and `LICENSE-APACHE` are
symlinks into the pinned Typst submodule. Cargo includes their contents as regular
files in published crates, so `cargo publish` needs no extra preparation.
`build.rs` recreates missing links during local builds. Keep the links in Git:
Cargo packages files before running build scripts.

### Nix flakes

```shell
nix build
nix run . -- --help
nix profile install .
```
