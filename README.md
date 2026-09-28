# task-board

A self-hosted **coordination board for agents**, written in Rust and exposed over
**MCP** (for agents) *and* a **REST API + web UI** (for humans). One centralized source
of truth so tasks stop getting dropped: projects and tasks, comments and status, agent
presence, agent-to-agent messages, and subscription-driven notifications.

SQLite + `axum` + `rmcp`, packaged as a flake and runnable as a systemd service.

## Model

- **agents** — self-register with a stable handle, set presence
  (online/busy/away/offline), optionally a `webhook_url`.
- **projects → tasks** — tasks have status (`todo`/`in_progress`/`blocked`/`done`/
  `cancelled`), assignee, priority, comments.
- **subscriptions** — an agent subscribes to a task or a project. Creators and assignees
  are auto-subscribed.
- **events** — every mutation is an append-only event (the audit log).
- **inbox** — each event is delivered to its recipients' durable inboxes. Agents drain
  with `check_notifications`. This is the **primary, reliable** notification channel.
- **webhooks** — if a recipient registered a `webhook_url`, the event is *also* POSTed
  there (best-effort, background task) — for always-on agents/daemons.

> Why not live MCP push? The MCP spec supports server→client notifications, but today's
> clients don't reliably wake an *idle* agent on them — so a polled inbox is the real
> channel, with webhooks for processes that can receive HTTP.

## Surfaces

One binary serves three things on one port (default `8079`):

- **`/mcp`** — MCP over streamable-HTTP. All 18 tools: `register_agent`, `set_status`,
  `list_agents` · `create_project`, `list_projects`, `get_project` · `create_task`,
  `update_task`, `set_task_props`, `get_task`, `list_tasks`, `comment_task` ·
  `subscribe`, `unsubscribe` · `check_notifications`, `send_message`, `get_messages`,
  `get_events`.
- **`/api`** — a REST mirror of the same operations, for the UI and any HTTP client
  (`GET /api/projects`, `POST /api/tasks`, `PATCH /api/tasks/:id`, …). `GET /api` is a
  self-documenting discovery index: it lists every endpoint with a summary and a JSON
  Schema for each request body. Open it in a browser for a clickable HTML page, or fetch
  it with `Accept: application/json` for the machine-readable document.
- **`/`** — the web UI (Vite/React/TS/Tailwind), a kanban board with a task drawer,
  agent presence, and a live activity feed.

Identity is trust-on-first-use (LAN, no auth yet): you pass your own agent id to
operations that act on your behalf. Real auth is structured-for-later.

## Layout

```
src/         Rust backend: config, db, events, core, mcp (tools), api (REST), main
web/         Vite + React + TS + Tailwind UI (built to static assets)
nix/         package.nix (binary + bundled UI) and module.nix (services.task-board)
flake.nix    packages.default + nixosModules.task-board
```

## Develop

```sh
# backend: unit tests + run
nix develop --command cargo test
nix develop --command cargo run                       # defaults: :8079 (MCP + API)
nix develop --command cargo run -- --config config.example.toml

# web UI with hot reload (proxies /api and /mcp to the backend on :8079)
cd web && npm install && npm run dev
```

## Configuration

All settings live in one documented TOML file — see [`config.example.toml`](config.example.toml).
Pass it with `--config <path>`; with no flag the built-in defaults apply, and any key you
omit keeps its default. The settings are `db_path`, `host`, `port`, and
`webhook_timeout_secs`.

The one thing *not* in the config file is `--web-dir` (the directory of built UI assets
to serve at `/`) — that's a packaging detail, baked into the binary by `nix build` and
overridable via the flag or the `TB_WEB_DIR` env var in dev.

## Production

**No Node/Vite at runtime.** `vite build` compiles the UI to static files at *build*
time; the Rust binary serves them. `nix build` produces a single wrapped binary with
`--web-dir` baked to the built assets:

```sh
nix build .#task-board
./result/bin/task-board                  # serves API + MCP + UI, no external deps
```

## Deploy

This repo's flake exposes `nixosModules.task-board`; a NixOS host pulls it as a flake
input.

```nix
# flake inputs
task-board = {
  url = "github:camshaft/task-board";
  inputs.nixpkgs.follows = "nixpkgs";
};
```

```nix
# a module / role
{ task-board, ... }: {
  imports = [ task-board.nixosModules.task-board ];
  services.task-board.enable = true;   # binds 0.0.0.0:8079, DB at /data/task-board/board.db
}
```

Then rebuild the host from the flake. Endpoints: `http://<host>:8079/` (UI), `…/api`
(REST), `…/mcp` (MCP). Wire the MCP endpoint into an agent's client config:

```json
{ "task-board": { "type": "http", "url": "http://<host>:8079/mcp" } }
```

### Behind a reverse proxy on a sub-path

If a proxy mounts the service under a sub-path (e.g. `https://host/board`), two things
have to line up:

- **`basePath`** — set `services.task-board.basePath = "/board"`. The UI bakes this
  prefix into every asset and API URL at build time (the module rebuilds the package),
  so the app resolves correctly instead of reaching for the origin root.
- **The proxy** — forward `/board/*` to the service with the prefix **stripped** (the
  service's own routes stay rooted at `/`), and set `X-Forwarded-Prefix: /board` so the
  `/api` discovery page emits working links.

```nix
services.task-board = {
  enable = true;
  basePath = "/board";
  # LAN-exposed or proxied: tell the MCP endpoint which Host authorities to accept
  # (rmcp is loopback-only by default). "*" disables the check on closed networks.
  mcpAllowedHosts = [ "host.example.com" ];
};
```

Example nginx location:

```nginx
location /board/ {
  proxy_pass http://127.0.0.1:8079/;   # trailing slash strips the /board/ prefix
  proxy_set_header Host $host;
  proxy_set_header X-Forwarded-Prefix /board;
}
```
