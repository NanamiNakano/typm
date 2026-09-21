{
  description = "An experimental package manager for Typst";

  inputs = {
    self.submodules = true;
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    naersk = {
      url = "github:nix-community/naersk";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { nixpkgs, naersk, ... }: {
    packages = nixpkgs.lib.genAttrs [
      "x86_64-linux"
      "aarch64-linux"
      "aarch64-darwin"
    ] (system:
      let
        cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
        name = cargoToml.package.name;
        version = cargoToml.package.version;
        pkgs = nixpkgs.legacyPackages.${system};
      in rec {
        default = typm;
        typm = (pkgs.callPackage naersk { }).buildPackage {
          inherit version name;
          src = pkgs.lib.fileset.toSource {
            root = ./.;
            fileset = pkgs.lib.fileset.unions [
              ./Cargo.toml ./Cargo.lock ./src ./tests ./README.md ./LICENSE ./typst-args
              ./vendor/typst/crates/typst-cli/src/args.rs
              ./vendor/typst/LICENSE
            ];
          };

          nativeBuildInputs = [ pkgs.installShellFiles ];
          OPENSSL_NO_VENDOR = 1;

          postInstall = ''
            installShellCompletion --cmd typm \
              --bash <("$out/bin/typm" completions bash) \
              --zsh <("$out/bin/typm" completions zsh) \
              --fish <("$out/bin/typm" completions fish)
          '';

          meta.mainProgram = "typm";
        };
      });
  };
}
