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

### Nix flakes

```shell
nix build
nix run . -- --help
nix profile install .
```
