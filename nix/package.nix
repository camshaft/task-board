{ lib, rustPlatform, buildNpmPackage, nodejs_22, makeWrapper }:

let
  # The web UI (Vite/React/TS/Tailwind) built to static assets. No Node at runtime —
  # this only runs at build time; the Rust binary serves the resulting `dist/`.
  web = buildNpmPackage {
    pname = "task-board-web";
    version = "0.1.0";
    src = ../web;
    npmDepsHash = "sha256-s+XX/td/mmF2C6iRM1v+cZruxWYF5Xdqv6ArK3SB8zI=";
    nodejs = nodejs_22;
    installPhase = ''
      runHook preInstall
      cp -r dist "$out"
      runHook postInstall
    '';
  };
in
rustPlatform.buildRustPackage {
  pname = "task-board";
  version = "0.1.0";
  src = lib.cleanSource ../.;
  cargoLock.lockFile = ../Cargo.lock;

  nativeBuildInputs = [ makeWrapper ];

  # Bake the built UI path in so the service serves the UI out of the box. Overridable
  # by setting TB_WEB_DIR in the environment.
  postInstall = ''
    wrapProgram "$out/bin/task-board" \
      --set-default TB_WEB_DIR "${web}"
  '';

  # Expose the raw web assets for consumers that want to serve them elsewhere.
  passthru.web = web;

  meta = {
    description = "SQLite-backed MCP + REST coordination board for agents";
    mainProgram = "task-board";
  };
}
