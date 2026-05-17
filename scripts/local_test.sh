#!/usr/bin/env bash
# local_test.sh — Track switching experiment (fully local, no SSH, no tc)
#
# Starts a local relay and a local publish-multi publisher (artificial bytes),
# then iterates over all three switching methods for N repetitions.
# No bandwidth shaping is applied; this is a baseline / smoke-test run.
#
# Usage:
#   bash scripts/local_test.sh [options]
#
# Options:
#   --build                Build release binaries before running
#   --skip-start           Assume relay + publisher are already running
#   --method  <name>       Only run this method (repeatable; default: all three)
#   --track-sequence <s>   Comma-separated track sequence, e.g. "2,3,4"
#                          When given, each run performs M switches (M = len-1).
#                          Default: "2,3" (single switch, 720p → 480p)
#   --switch-after <secs>  Seconds per track before triggering next switch (default: 15)
#   --jitter-buffer-ms <ms>  Jitter buffer for realtime freeze calculation (default: 0)
#   --reps <n>             Repetitions per method (default: 3)
#   --tracks <spec>        Track specs for publish-multi, e.g. "1:20000,2:12500,3:5000"
#                          Default: full 5-track ladder matching ffmpeg.sh bitrates
#   --objects-per-group <n>  Objects per group (default: 25, i.e. 1s GOP at 25fps)
#   --interval <ms>        Inter-object interval in ms (default: 40, i.e. 25fps)
#   --group-count <n>      Total groups to publish (default: 1000 ≈ ~17 minutes)
#   --relay-port <port>    Relay QUIC port (default: 4433)
#   --output <dir>         Results directory (default: results/local_YYYYMMDD_HHMMSS)
#   --help
#
# Examples:
#   # Quick smoke test: one method, one rep, relay already running
#   bash scripts/local_test.sh --skip-start --method switch-message --reps 1
#
#   # Full run: build + 3 methods × 3 reps (54 switch events with default 3-track sequence)
#   bash scripts/local_test.sh --build --track-sequence "2,3,4"
#
#   # Single-switch baseline
#   bash scripts/local_test.sh --track-sequence "2,3" --reps 5

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

# ── Defaults ───────────────────────────────────────────────────────────────────

ALL_METHODS=("switch-message" "sub-update-forward" "joining-fetch")
# Default track sequence: single upswitch 480p→720p (lower number = lower bitrate)
TRACK_SEQUENCE="2,3"
SWITCH_AFTER=15
JITTER_BUFFER_MS=0
REPS=3
RELAY_PORT=4433
RELAY_URL="https://127.0.0.1:${RELAY_PORT}"
NAMESPACE="moqtail-experiment"

# publish-multi defaults — video-only bitrate ladder (lower track = lower bitrate):
#   track 1: 500 kbps (640×360)   → 2500 B/obj at 25fps
#   track 2: 1 Mbps   (854×480)   → 5000 B/obj
#   track 3: 2.5 Mbps (1280×720)  → 12500 B/obj
#   track 4: 4 Mbps   (1920×1080) → 20000 B/obj
PUB_TRACKS="1:2500,2:5000,3:12500,4:20000"
PUB_OBJECTS_PER_GROUP=25
PUB_INTERVAL_MS=40
PUB_GROUP_COUNT=1000

# ── Paths ──────────────────────────────────────────────────────────────────────

CLIENT_BIN="$ROOT_DIR/target/release/client"
RELAY_BIN="$ROOT_DIR/target/release/relay"
RELAY_LOG="/tmp/moqtail-relay-local.log"
RELAY_PID_FILE="/tmp/moqtail-relay-local.pid"
PUB_LOG="/tmp/moqtail-pub-local.log"
PUB_PID_FILE="/tmp/moqtail-pub-local.pid"

# ── Arg parsing ────────────────────────────────────────────────────────────────

BUILD=false
SKIP_START=false
SELECTED_METHODS=()
OUTPUT_DIR=""

usage() {
  grep '^#' "$0" | sed 's/^# \{0,1\}//' | tail -n +2
  exit 0
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --build)               BUILD=true;                          shift ;;
    --skip-start)          SKIP_START=true;                     shift ;;
    --method)              SELECTED_METHODS+=("$2");             shift 2 ;;
    --track-sequence)      TRACK_SEQUENCE="$2";                 shift 2 ;;
    --switch-after)        SWITCH_AFTER="$2";                   shift 2 ;;
    --jitter-buffer-ms)    JITTER_BUFFER_MS="$2";               shift 2 ;;
    --reps)                REPS="$2";                           shift 2 ;;
    --tracks)              PUB_TRACKS="$2";                     shift 2 ;;
    --objects-per-group)   PUB_OBJECTS_PER_GROUP="$2";          shift 2 ;;
    --interval)            PUB_INTERVAL_MS="$2";                shift 2 ;;
    --group-count)         PUB_GROUP_COUNT="$2";                shift 2 ;;
    --relay-port)          RELAY_PORT="$2"; RELAY_URL="https://127.0.0.1:${RELAY_PORT}"; shift 2 ;;
    --output)              OUTPUT_DIR="$2";                     shift 2 ;;
    --help|-h)             usage ;;
    *) echo "Unknown option: $1" >&2; exit 1 ;;
  esac
done

[ ${#SELECTED_METHODS[@]} -eq 0 ] && METHODS=("${ALL_METHODS[@]}") || METHODS=("${SELECTED_METHODS[@]}")
[ -z "$OUTPUT_DIR" ] && OUTPUT_DIR="$ROOT_DIR/results/local_$(date +%Y%m%d_%H%M%S)"

# ── Helpers ────────────────────────────────────────────────────────────────────

log() { echo "[$(date +%H:%M:%S)] $*"; }
die() { echo "ERROR: $*" >&2; exit 1; }

# ── Build ──────────────────────────────────────────────────────────────────────

do_build() {
  log "Building release binaries (relay + client)..."
  cargo build --release --bin relay --bin client \
    --manifest-path "$ROOT_DIR/Cargo.toml" 2>&1 | tail -10
  log "Build complete."
}

# ── Relay ──────────────────────────────────────────────────────────────────────

start_relay() {
  if [ ! -x "$RELAY_BIN" ]; then
    die "Relay binary not found at $RELAY_BIN — run with --build first"
  fi

  log "Stopping any lingering local relay..."
  pkill -f "target/release/relay" 2>/dev/null || true
  sleep 1

  log "Starting local relay (port $RELAY_PORT) → $RELAY_LOG"
  "$RELAY_BIN" --port "$RELAY_PORT" >"$RELAY_LOG" 2>&1 &
  echo $! >"$RELAY_PID_FILE"
  sleep 2
  log "Relay PID $(cat "$RELAY_PID_FILE")"
}

stop_relay() {
  if [ -f "$RELAY_PID_FILE" ]; then
    log "Stopping relay (PID $(cat "$RELAY_PID_FILE"))..."
    kill "$(cat "$RELAY_PID_FILE")" 2>/dev/null || true
    rm -f "$RELAY_PID_FILE"
  fi
}

# ── Publisher ──────────────────────────────────────────────────────────────────

start_publisher() {
  if [ ! -x "$CLIENT_BIN" ]; then
    die "Client binary not found at $CLIENT_BIN — run with --build first"
  fi

  log "Stopping any lingering local publisher..."
  # kill any previous publish-multi client (best-effort)
  pkill -f "client.*publish-multi" 2>/dev/null || true
  sleep 1

  log "Starting publish-multi (tracks=$PUB_TRACKS, groups=$PUB_GROUP_COUNT) → $PUB_LOG"
  "$CLIENT_BIN" \
    --server "$RELAY_URL" \
    --namespace "$NAMESPACE" \
    --command publish-multi \
    --no-cert-validation \
    --tracks "$PUB_TRACKS" \
    --objects-per-group "$PUB_OBJECTS_PER_GROUP" \
    --interval "$PUB_INTERVAL_MS" \
    --group-count "$PUB_GROUP_COUNT" \
    >"$PUB_LOG" 2>&1 &
  echo $! >"$PUB_PID_FILE"

  # Wait a moment so the relay has received at least a couple of groups
  # from every track before the first switch-test run starts.
  local warmup=$(( SWITCH_AFTER > 5 ? 5 : SWITCH_AFTER ))
  log "Publisher PID $(cat "$PUB_PID_FILE") — waiting ${warmup}s for initial cache warm-up..."
  sleep "$warmup"
}

stop_publisher() {
  if [ -f "$PUB_PID_FILE" ]; then
    log "Stopping publisher (PID $(cat "$PUB_PID_FILE"))..."
    kill "$(cat "$PUB_PID_FILE")" 2>/dev/null || true
    rm -f "$PUB_PID_FILE"
  fi
}

# ── Single run ─────────────────────────────────────────────────────────────────

run_one() {
  local method=$1 rep=$2
  local label="${method}_0bps"
  local outfile="$OUTPUT_DIR/${label}_rep${rep}.json"

  log "--- $method rep $rep ---"

  "$CLIENT_BIN" \
    --server "$RELAY_URL" \
    --namespace "$NAMESPACE" \
    --command switch-test \
    --no-cert-validation \
    --track-sequence "$TRACK_SEQUENCE" \
    --method "$method" \
    --switch-after "$SWITCH_AFTER" \
    --jitter-buffer-ms "$JITTER_BUFFER_MS" \
    --bandwidth-cap-bps 0 \
    --output-json "$outfile" \
    && log "Saved: $outfile" \
    || log "WARNING: switch-test exited with error ($method rep $rep)"

  sleep 2
}

# ── Cleanup on exit ────────────────────────────────────────────────────────────

cleanup() {
  log "Cleaning up..."
  stop_publisher
  stop_relay
}
trap cleanup EXIT INT TERM

# ── Main ───────────────────────────────────────────────────────────────────────

main() {
  log "=== local_test.sh ==="
  log "Methods:        ${METHODS[*]}"
  log "Track sequence: $TRACK_SEQUENCE"
  log "Switch after:   ${SWITCH_AFTER}s"
  log "Jitter buffer:  ${JITTER_BUFFER_MS}ms"
  log "Reps:           $REPS"
  log "Publisher:      local (publish-multi)"
  log "Relay:          local ($RELAY_URL)"

  mkdir -p "$OUTPUT_DIR"
  log "Results:        $OUTPUT_DIR"

  if "$BUILD"; then
    do_build
  fi

  if ! "$SKIP_START"; then
    start_relay
    start_publisher
  fi

  local total=$(( ${#METHODS[@]} * REPS ))
  local done_count=0
  log "Running $total experiment runs..."

  for method in "${METHODS[@]}"; do
    for rep in $(seq 1 "$REPS"); do
      run_one "$method" "$rep"
      (( done_count++ )) || true
      log "Progress: $done_count / $total"
    done
  done

  log "All done. Results in $OUTPUT_DIR"
  # cleanup via trap
}

main
