{
  description = "task-board: SQLite-backed MCP + REST coordination board for agents";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.05";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    { self, nixpkgs, flake-utils, rust-overlay }:
    let
      # System-independent bits: the package builder and the NixOS module. Consumers
      # (the dotfiles flake) pull `nixosModules.task-board` just like the capmeshd input.
      # rmcp 3.5 needs rustc >= 1.88, newer than nixpkgs 25.05 ships, so we build with a
      # pinned stable toolchain from rust-overlay.
      overlay = final: prev:
        let
          rust = final.rust-bin.stable."1.90.0".default;
          rustPlatform = final.makeRustPlatform {
            cargo = rust;
            rustc = rust;
          };
        in
        {
          task-board = final.callPackage ./nix/package.nix { inherit rustPlatform; };
        };
    in
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) overlay ];
        };
        rust = pkgs.rust-bin.stable."1.90.0".default.override {
          extensions = [ "rust-src" "rust-analyzer" "clippy" "rustfmt" ];
        };
      in
      {
        packages.default = pkgs.task-board;
        packages.task-board = pkgs.task-board;

        devShells.default = pkgs.mkShell {
          packages = [
            rust
            pkgs.nodejs_22
            pkgs.sqlite # sqlite3 CLI for poking at the DB
          ];
          shellHook = ''echo "task-board devShell: $(rustc --version), node $(node --version)" >&2'';
        };
      }
    )
    // {
      overlays.default = overlay;

      # capmesh-style: `imports = [ task-board.nixosModules.task-board ]` then
      # `services.task-board.enable = true` in a dotfiles role.
      nixosModules.task-board = import ./nix/module.nix self;
    };
}
