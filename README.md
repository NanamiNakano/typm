# typm

`typm` is an experimental package manager for `typst` that mimic `cargo` to manage local dependencies and unpublished
dependencies from git.

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

### Configuration

`typm` reads `typm.toml` in your project root.

```toml
[dependencies]
mathtools = { git = "https://github.com/NanamiNakano/mathtools.git", tag = "v0.1.2" }

[options]
outdir = "out"
```

## Install

### Build from Source

```shell
git submodule update --init -- vendor/typst
cargo build --release
```
