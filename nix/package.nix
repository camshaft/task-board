{ lib, rustPlatform, buildNpmPackage, nodejs_22, makeWrapper }:

let
  # The web UI (Vite/React/TS/Tailwind) built to static assets. No Node at runtime —
  # this only runs at build time; the Rust binary serves the resulting `dist/`. The build
  # is mount-point agnostic (relative asset URLs); serving under a sub-path is handled at
  # runtime by the backend via X-Forwarded-Prefix, so nothing is baked in here.
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
