# typm

`typm` is an experimental package manager for `typst` that mimic `cargo`.

## Install

```shell
cargo install typm
```

To build a Git checkout, initialize the pinned Typst submodule first:

```shell
git submodule update --init -- vendor/typst
cargo build --release
```

Completion generation compiles the CLI definitions directly from the pinned
`vendor/typst` submodule. No extra source copies or symlinks are needed.

## Shell completions

`typm completions <shell>` prints a completion script for typm's package commands
and its wrapped Typst commands, including their options and aliases. Supported
shells are Bash, Zsh, Fish, PowerShell, and Elvish.

For Bash, add this to `~/.bashrc`:

```bash
source <(typm completions bash)
```

For Zsh, save the script to a directory on your completion search path:

```zsh
mkdir -p ~/.zsh/completions
typm completions zsh > ~/.zsh/completions/_typm
```

Add `fpath=(~/.zsh/completions $fpath)` to `~/.zshrc` before your existing
`compinit` call. If you do not already initialize completions, follow it with
`autoload -Uz compinit && compinit`.

For Fish:

```fish
mkdir -p ~/.config/fish/completions
typm completions fish > ~/.config/fish/completions/typm.fish
```

For PowerShell, add this to your `$PROFILE`:

```powershell
typm completions powershell | Out-String | Invoke-Expression
```

For Elvish, save the script as a module:

```elvish
mkdir -p ~/.config/elvish/lib
typm completions elvish > ~/.config/elvish/lib/typm-completions.elv
```

Add `use typm-completions` to `~/.config/elvish/rc.elv`.

Restart your shell after setup. Regenerate saved scripts after updating typm;
profile commands regenerate them when a new shell starts.

Typst completion definitions are bundled from **Typst 0.15.1**. The default
`embedded-fonts`, `http-server`, and `self-update` Cargo features include its
optional completion metadata; they do not enable Typst runtime features in typm.
Generating completions does not require Typst or a project manifest. Suggestions
follow the bundled version; execution still forwards Typst arguments unchanged
to the installed binary. `typm update` and `typm completions` use typm's commands
when names overlap.

The bundled definitions retain their Apache-2.0 license; typm's original source
is MIT-licensed. See [Typst's license](vendor/typst/LICENSE).

## Updating the Typst definitions

The submodule is pinned to tag `v0.15.1`, commit
`9dfd3a08500b7896045f907433cf7b4b02434fad`. To select another released tag,
replace `v0.15.1` in these commands:

```shell
git -C vendor/typst fetch origin tag v0.15.1
git -C vendor/typst checkout --detach v0.15.1
git add vendor/typst
```

Update the exact `typst-utils` dependency and the documented version and commit
alongside the submodule. Review upstream's CLI imports, feature gates, license,
and NOTICE, then run the completion tests. Keep the upstream source unchanged;
the module is excluded from Rust formatting for that reason.

## Packaging from source

Cargo excludes nested projects containing `Cargo.toml`, so packaging from a full
Typst checkout omits the CLI definitions. For releases, temporarily narrow a
clean, fully initialized submodule to the two upstream files used by typm:

```shell
(
    set -e
    trap 'git -C vendor/typst sparse-checkout disable' EXIT
    git -C vendor/typst sparse-checkout set --no-cone \
        /LICENSE /crates/typst-cli/src/args.rs
    cargo package --locked
)
```

The checkout is restored when the subshell exits, including after a failed
package build. The archive contains the unchanged upstream files at their
original paths and builds without Git or a submodule. When publishing, run
`cargo publish --locked` in place of the package command inside the same
subshell. Normal development builds use the full submodule directly.
