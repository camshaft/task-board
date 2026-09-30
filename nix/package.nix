{ lib, rustPlatform, buildNpmPackage, nodejs_22, makeWrapper, rev ? "dev" }:

let
  # The web UI (Vite/React/TS/Tailwind) built to static assets. No Node at runtime —
  # this only runs at build time; the Rust binary serves the resulting `dist/`. The build
  # is mount-point agnostic (relative asset URLs); serving under a sub-path is handled at
  # runtime by the backend via X-Forwarded-Prefix, so nothing is baked in here.
  web = buildNpmPackage {
    pname = "task-board-web";
    version = "0.1.0";
    src = ../web;
    npmDepsHash = "sha256-tKBnXAGD964svAUa801ZddBsbX76S4YjAISOgYNMrgM=";
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

  # Bake the build's git rev into the binary (read via option_env! and reported at /api/health)
  # so an agent can confirm its just-merged commit is the live one instead of blind-polling.
  TASK_BOARD_COMMIT = rev;

  nativeBuildInputs = [ makeWrapper ];

  # Bake the built UI path in so the service serves the UI out of the box. This is a
  # packaging detail (not a user setting), so it's a default CLI arg, not config.
  postInstall = ''
    wrapProgram "$out/bin/task-board" \
      --add-flags "--web-dir ${web}"
  '';

  # Expose the raw web assets for consumers that want to serve them elsewhere.
  passthru.web = web;

  meta = {
    description = "SQLite-backed MCP + REST coordination board for agents";
    mainProgram = "task-board";
  };
}
