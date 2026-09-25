# typm

`typm` is an experimental package manager for `typst` that mimic `cargo`.

## Usage

```shell
typm add --path ../my-package
typm sync
typm compile main.typ
```

Direct dependencies declared in your project's `typm.toml` use version `0.0.0`
in Typst imports:

```typst
#import "@typm/my-package:0.0.0": *
```

## Install

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
