# `basePath` is the public URL prefix the UI is served under (default "/"). Set it to
# e.g. "/board" when the service sits behind a reverse proxy on a sub-path: it's baked
# into the built assets, so it must be chosen at build time. The NixOS module passes the
# value of `services.task-board.basePath` here via an override.
{ lib, rustPlatform, buildNpmPackage, nodejs_22, makeWrapper, basePath ? "/" }:

let
  # The web UI (Vite/React/TS/Tailwind) built to static assets. No Node at runtime —
  # this only runs at build time; the Rust binary serves the resulting `dist/`.
  web = buildNpmPackage {
    pname = "task-board-web";
    version = "0.1.0";
    src = ../web;
    npmDepsHash = "sha256-s+XX/td/mmF2C6iRM1v+cZruxWYF5Xdqv6ArK3SB8zI=";
    nodejs = nodejs_22;
    # Vite reads VITE_BASE_PATH (see web/vite.config.ts) to prefix every asset/API URL.
    VITE_BASE_PATH = basePath;
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
