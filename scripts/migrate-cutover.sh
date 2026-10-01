#!/usr/bin/env bash
#
# migrate-cutover.sh -- board DB migration from green to the co-resident dev-dsk (task_950).
#
# THIS SCRIPT IS OPERATOR-RUN AND REVIEWABLE BY DESIGN. Per cameron (task_950): the flip is a
# runnable script we can review, NOT a cutover an agent drives. Read it top-to-bottom before you
# run it. Every irreversible step pauses for an explicit "yes". Nothing here runs automatically.
#
# It implements cameron's plan in two modes:
#   --rehearse  (default)  Pull a snapshot of the LIVE green board over the secure tunnel (or
#                          loopback), verify it, load it into a dev-dsk board fork, and start that
#                          fork so you can debug it. NO flip -- green stays the live board. Repeat
#                          as many times as you like until the dev-dsk fork is happy.
#   --flip                 The real cutover: ask the fleet to pause, pull a FRESH snapshot, verify,
#                          load it into the dev-dsk board, start it, point the proxy at the dev-dsk
#                          board, verify end-to-end, then tell the fleet to resume. Rollback at the
#                          bottom (point the proxy back at green).
#
# Prerequisites on green (crate side, shipped): the authenticated snapshot endpoint
# (GET /api/admin/db-snapshot, PR #291) and the NixOS module options (PR #293). green-machine-ops
# enables it with a temporary basic-auth cred for the cutover window and disables it after.
#
# ------------------------------------------------------------------------------------------------
# REVIEW CHECKLIST -- the values below are the infra-owned blanks. green-machine-ops / v-nix should
# confirm each against the real dev-dsk + proxy before this is run. They are environment-overridable
# so the script itself needs no edit at run time:  GREEN_BOARD_URL=... ./migrate-cutover.sh --flip
# ------------------------------------------------------------------------------------------------
set -euo pipefail

# --- Source board (the LIVE green board's snapshot endpoint) ------------------------------------
# cameron's plan pulls over the secure tunnel from the dev-dsk; green-machine-ops's alternative is a
# loopback pull on green then a LAN transfer. Set this to whichever topology cutover uses:
#   tunnel:    https://green-machine.camshaft.dev/board   (CF-Access-fronted; dev-dsk self-pull)
#   loopback:  http://127.0.0.1:8079                      (run on green, then transfer the file)
GREEN_BOARD_URL="${GREEN_BOARD_URL:-https://green-machine.camshaft.dev/board}"
SNAPSHOT_USER="${SNAPSHOT_USER:-ops}"
# Never bake the password into the script. Supply it at run time: SNAPSHOT_PASS=... ./...
SNAPSHOT_PASS="${SNAPSHOT_PASS:-}"

# --- Destination (the dev-dsk board) ------------------------------------------------------------
# Where the dev-dsk board reads its DB, and the systemd units that run it (mirrors green's two-unit
# socket+service shape). Confirm the real unit names + db_path on the dev-dsk deploy.
DEV_DSK_DB_PATH="${DEV_DSK_DB_PATH:-/data/task-board/board.db}"
DEV_DSK_BOARD_SERVICE="${DEV_DSK_BOARD_SERVICE:-task-board.service}"
DEV_DSK_BOARD_SOCKET="${DEV_DSK_BOARD_SOCKET:-task-board.socket}"
DEV_DSK_BOARD_HEALTH="${DEV_DSK_BOARD_HEALTH:-http://127.0.0.1:8079/api/health}"

# --- Rehearsal fork (a throwaway board on the dev-dsk; must NOT be the real db_path) -------------
REHEARSE_DB_PATH="${REHEARSE_DB_PATH:-/tmp/task-board-rehearsal/board.db}"

# --- Proxy flip (the step that makes the fleet talk to the dev-dsk board) -----------------------
# cameron: "add a rule to the caddy proxy to forward to the local board." The /board/* reverse-proxy
# directives are the ones from README.md (NOTE the HTTP/1.1 pin -- Caddy negotiates HTTP/2 to
# upstreams by default, which SILENTLY DROPS the /board/tunnel/ws WebSocket upgrade). Confirm the
# real caddy config path + reload command on whichever host fronts /board/ after cutover.
CADDY_CONFIG="${CADDY_CONFIG:-/etc/caddy/Caddyfile}"
CADDY_RELOAD_CMD="${CADDY_RELOAD_CMD:-sudo systemctl reload caddy}"
# The upstream the proxy should point /board/* at AFTER the flip (the dev-dsk board):
NEW_BOARD_UPSTREAM="${NEW_BOARD_UPSTREAM:-127.0.0.1:8079}"

# --- sudo wrapper (NixOS: the setuid wrapper, not /run/current-system/sw/bin/sudo) --------------
SUDO="${SUDO:-/run/wrappers/bin/sudo}"

SNAPSHOT_OUT="${SNAPSHOT_OUT:-./board-snapshot-$(date +%Y%m%dT%H%M%SZ).db}"

# ================================================================================================
log()  { printf '\n=== %s ===\n' "$*" >&2; }
die()  { printf 'ERROR: %s\n' "$*" >&2; exit 1; }
confirm() {
  # Explicit, interactive gate before anything irreversible. Refuses to proceed on a non-tty so the
  # script can never auto-run a destructive step unattended.
  [[ -t 0 ]] || die "refusing to proceed non-interactively at: $1"
  local reply
  read -r -p ">>> $1 Type 'yes' to proceed: " reply
  [[ "$reply" == "yes" ]] || die "aborted at: $1"
}

preflight() {
  log "Preflight"
  command -v curl    >/dev/null || die "curl not found"
  command -v sqlite3 >/dev/null || die "sqlite3 not found (needed for integrity_check)"
  [[ -n "$SNAPSHOT_PASS" ]] || die "set SNAPSHOT_PASS (the temporary snapshot basic-auth password)"
  # The endpoint should answer (200 with creds). A 404 means it is not enabled on green yet.
  local code
  code="$(curl -sS -o /dev/null -w '%{http_code}' -u "$SNAPSHOT_USER:$SNAPSHOT_PASS" \
    "$GREEN_BOARD_URL/api/admin/db-snapshot" || true)"
  case "$code" in
    200) echo "snapshot endpoint reachable + authed (200)" >&2 ;;
    401) die "401 from the snapshot endpoint -- wrong SNAPSHOT_USER/SNAPSHOT_PASS" ;;
    404) die "404 -- the snapshot endpoint is not enabled on green (set dbSnapshotEnabled=true)" ;;
    *)   die "unexpected HTTP $code from $GREEN_BOARD_URL/api/admin/db-snapshot" ;;
  esac
}

pull_snapshot() {
  log "Pull snapshot -> $SNAPSHOT_OUT"
  curl -fsS -u "$SNAPSHOT_USER:$SNAPSHOT_PASS" \
    "$GREEN_BOARD_URL/api/admin/db-snapshot" -o "$SNAPSHOT_OUT"
  # Belt-and-suspenders: the server already integrity-checks before serving, but verify locally too.
  head -c 16 "$SNAPSHOT_OUT" | grep -q "SQLite format 3" || die "downloaded file is not a SQLite DB"
  local ic; ic="$(sqlite3 "$SNAPSHOT_OUT" 'PRAGMA integrity_check;')"
  [[ "$ic" == "ok" ]] || die "integrity_check failed on the snapshot: $ic"
  echo "snapshot verified: SQLite magic + PRAGMA integrity_check = ok" >&2
}

start_board() {  # $1 = db_path to load the snapshot into, $2 = service, $3 = socket
  local db_path="$1" service="$2" socket="$3"
  log "Load snapshot into $db_path and start $service"
  $SUDO install -D -m 0640 "$SNAPSHOT_OUT" "$db_path"
  # A fresh snapshot (VACUUM INTO output) has no sidecar WAL/SHM to carry; it opens clean.
  $SUDO systemctl start "$socket" "$service"
  # Health-check: wait for 200 and print the served commit so you can confirm the right build.
  for _ in $(seq 1 30); do
    if curl -fsS "$DEV_DSK_BOARD_HEALTH" >/dev/null 2>&1; then
      echo "dev-dsk board healthy: $(curl -fsS "$DEV_DSK_BOARD_HEALTH")" >&2
      return 0
    fi
    sleep 1
  done
  die "dev-dsk board did not become healthy at $DEV_DSK_BOARD_HEALTH"
}

flip_proxy() {
  log "Flip the proxy /board/* -> $NEW_BOARD_UPSTREAM (the dev-dsk board)"
  echo "The /board/* reverse-proxy block must point at the dev-dsk board. Expected Caddy form"  >&2
  echo "(mind the HTTP/1.1 pin so the /board/tunnel/ws WebSocket upgrade survives):"            >&2
  cat >&2 <<EOF

  handle_path /board/* {
    reverse_proxy $NEW_BOARD_UPSTREAM {
      header_up X-Forwarded-Prefix /board
      transport http { versions 1.1 }
    }
  }

EOF
  echo "Edit $CADDY_CONFIG to the above (keep a backup), then this reloads it:" >&2
  confirm "Reload the proxy to point /board/* at $NEW_BOARD_UPSTREAM?"
  $CADDY_RELOAD_CMD
  echo "proxy reloaded" >&2
}

verify_end_to_end() {
  log "Verify /board/ now serves the dev-dsk board"
  local health; health="$(curl -fsS "$GREEN_BOARD_URL/api/health")" || die "/board/api/health failed post-flip"
  echo "/board/api/health -> $health" >&2
  echo "Confirm the 'commit' above matches the dev-dsk build, and that the UI + /mcp work." >&2
}

announce() { log "FLEET: $*"; echo "(do this now, then continue)" >&2; confirm "$1 done?"; }

usage() { grep '^#' "$0" | sed 's/^# \{0,1\}//'; exit "${1:-0}"; }

MODE="--rehearse"
[[ "${1:-}" == "--help" || "${1:-}" == "-h" ]] && usage 0
[[ -n "${1:-}" ]] && MODE="$1"

case "$MODE" in
  --rehearse)
    preflight
    pull_snapshot
    start_board "$REHEARSE_DB_PATH" "$DEV_DSK_BOARD_SERVICE" "$DEV_DSK_BOARD_SOCKET"
    log "REHEARSAL COMPLETE -- green is still the live board. Debug the dev-dsk fork, then re-run."
    ;;
  --flip)
    preflight
    announce "Ask every fleet agent to PAUSE writes to the board (fleet-wide quiesce)."
    pull_snapshot   # fresh, as of the pause, so no writes are lost
    start_board "$DEV_DSK_DB_PATH" "$DEV_DSK_BOARD_SERVICE" "$DEV_DSK_BOARD_SOCKET"
    flip_proxy
    verify_end_to_end
    announce "Tell the fleet to RESUME -- /board/ now serves the dev-dsk board."
    log "FLIP COMPLETE."
    cat >&2 <<'EOF'

ROLLBACK (if the dev-dsk board misbehaves after the flip):
  1. Point the proxy /board/* back at the green board upstream and reload it.
  2. The green board was only quiesced (socket+service stopped), never destroyed -- restart it:
       sudo systemctl start task-board.socket task-board.service   # on green
  3. Tell the fleet to resume against green. No data is lost: green's DB was never modified.
EOF
    ;;
  *) echo "unknown mode: $MODE" >&2; usage 1 ;;
esac
