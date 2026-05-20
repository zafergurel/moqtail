#!/usr/bin/env bash
# experiment.sh — Track switching experiment (remote relay, local publish-multi publisher)
#
# Runs on the subscriber machine. Starts the relay on a remote SSH host and a
# local publish-multi publisher (artificial bytes, same as local_test.sh), then
# iterates over a method × bandwidth matrix, saving one JSON file per run.
#
# Configuration is read from scripts/.env (gitignored). Copy
# scripts/.env.example to scripts/.env and fill in your values.
#
# Usage:
#   bash scripts/experiment.sh [options]
#
# Options:
#   --build              Build release binaries on relay and client locally
#   --skip-start         Assume relay + publisher are already running
#   --method  <name>     Only run this method (repeatable; default: all three)
#   --bandwidth <bps>    Only run at this bandwidth; 0 = no limit (repeatable; default: all)
#   --track-sequence <s> Comma-separated track sequence, e.g. "2,3,4,3,2"
#                        Default: "2,3,4,3,2" (up-up-down-down across 4 bitrates)
#   --switch-after <s>   Seconds per track before triggering next switch (default: 15)
#   --jitter-buffer-ms <ms>  Jitter buffer for realtime freeze calculation (default: 40)
#   --reps <n>           Repetitions per condition (default: 3)
#   --tracks <spec>      Track specs for publish-multi, e.g. "1:2500,2:5000,3:12500,4:20000"
#   --objects-per-group <n>  Objects per group (default: 25, i.e. 1s GOP at 25fps)
#   --interval <ms>      Inter-object interval in ms (default: 40, i.e. 25fps)
#   --group-count <n>    Total groups to publish (default: 1000 ≈ ~17 minutes)
#   --output <dir>       Results directory (default: results/YYYYMMDD_HHMMSS)
#   --help
#
# Examples:
#   # Baseline only (no bandwidth shaping), 3 reps per method
#   bash scripts/experiment.sh --bandwidth 0
#
#   # Full matrix: all bandwidths × all methods × 3 reps
#   bash scripts/experiment.sh --build

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

# ── Load environment ───────────────────────────────────────────────────────────

ENV_FILE="$SCRIPT_DIR/.env"
if [ ! -f "$ENV_FILE" ]; then
  echo "ERROR: $ENV_FILE not found. Copy scripts/.env.example to scripts/.env and fill in your values." >&2
  exit 1
fi
# shellcheck source=/dev/null
source "$ENV_FILE"

: "${RELAY_SSH:?RELAY_SSH must be set in scripts/.env}"
: "${RELAY_PROJECT:?RELAY_PROJECT must be set in scripts/.env}"

RELAY_PORT="${RELAY_PORT:-4433}"
RELAY_HOST_IP="${RELAY_SSH##*@}"
RELAY_URL="https://${RELAY_HOST_IP}:${RELAY_PORT}"
NAMESPACE="${NAMESPACE:-moqtail-experiment}"

# Optional: run publisher on a remote host instead of locally.
# If PUB_SSH is unset, publisher runs on this machine.
PUB_SSH="${PUB_SSH:-}"
PUB_PROJECT="${PUB_PROJECT:-$ROOT_DIR}"
# If publisher is on the relay host, it connects via loopback.
if [ -n "$PUB_SSH" ] && [ "$PUB_SSH" = "$RELAY_SSH" ]; then
  PUB_RELAY_URL="https://127.0.0.1:${RELAY_PORT}"
else
  PUB_RELAY_URL="$RELAY_URL"
fi

# ── Experiment defaults ────────────────────────────────────────────────────────

ALL_METHODS=("switch-message" "sub-update-forward" "joining-fetch")
# Bandwidth ladder (bps). 0 = unlimited baseline (no tc rule applied).
ALL_BANDWIDTHS=(0 5000000 3000000 2000000 1500000 1000000)
# Track sequence: lower index = lower bitrate
#   track 1=500kbps, 2=1Mbps, 3=2.5Mbps, 4=4Mbps
TRACK_SEQUENCE="2,3,4,3,2"
SWITCH_AFTER=15
JITTER_BUFFER_MS=40
REPS=3
TC_MARK=1

# publish-multi defaults — video-only bitrate ladder (lower track = lower bitrate):
#   track 1: 500 kbps (640×360)   → 2500 B/obj at 25fps
#   track 2: 1 Mbps   (854×480)   → 5000 B/obj
#   track 3: 2.5 Mbps (1280×720)  → 12500 B/obj
#   track 4: 4 Mbps   (1920×1080) → 20000 B/obj
PUB_TRACKS="1:2500,2:5000,3:12500,4:20000"
PUB_OBJECTS_PER_GROUP=25
PUB_INTERVAL_MS=40
PUB_GROUP_COUNT=5000

# ── Paths ──────────────────────────────────────────────────────────────────────

CLIENT_BIN="$ROOT_DIR/target/release/client"
PUB_LOG="/tmp/moqtail-pub.log"
PUB_PID_FILE="/tmp/moqtail-pub.pid"

# ── Flag overrides ─────────────────────────────────────────────────────────────

BUILD=false
SKIP_START=false
SELECTED_METHODS=()
SELECTED_BANDWIDTHS=()
OUTPUT_DIR=""

usage() {
  grep '^#' "$0" | sed 's/^# \{0,1\}//' | tail -n +2
  exit 0
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --build)             BUILD=true;                          shift ;;
    --skip-start)        SKIP_START=true;                     shift ;;
    --method)            SELECTED_METHODS+=("$2");             shift 2 ;;
    --bandwidth)         SELECTED_BANDWIDTHS+=("$2");          shift 2 ;;
    --track-sequence)    TRACK_SEQUENCE="$2";                  shift 2 ;;
    --switch-after)      SWITCH_AFTER="$2";                   shift 2 ;;
    --jitter-buffer-ms)  JITTER_BUFFER_MS="$2";               shift 2 ;;
    --reps)              REPS="$2";                           shift 2 ;;
    --tracks)            PUB_TRACKS="$2";                     shift 2 ;;
    --objects-per-group) PUB_OBJECTS_PER_GROUP="$2";          shift 2 ;;
    --interval)          PUB_INTERVAL_MS="$2";                shift 2 ;;
    --group-count)       PUB_GROUP_COUNT="$2";                shift 2 ;;
    --output)            OUTPUT_DIR="$2";                     shift 2 ;;
    --help|-h)           usage ;;
    *) echo "Unknown option: $1" >&2; exit 1 ;;
  esac
done

[ ${#SELECTED_METHODS[@]}    -eq 0 ] && METHODS=("${ALL_METHODS[@]}")    || METHODS=("${SELECTED_METHODS[@]}")
[ ${#SELECTED_BANDWIDTHS[@]} -eq 0 ] && BANDWIDTHS=("${ALL_BANDWIDTHS[@]}") || BANDWIDTHS=("${SELECTED_BANDWIDTHS[@]}")
[ -z "$OUTPUT_DIR" ] && OUTPUT_DIR="$ROOT_DIR/results/$(date +%Y%m%d_%H%M%S)"

# ── Resume / fresh prompt ──────────────────────────────────────────────────────
# If the output directory already has results, ask whether to resume or start fresh.

if [ -d "$OUTPUT_DIR" ]; then
  existing=$(find "$OUTPUT_DIR" -maxdepth 1 -name '*.json' | wc -l)
  total=$(( ${#METHODS[@]} * ${#BANDWIDTHS[@]} * REPS ))
  if [ "$existing" -gt 0 ] && [ "$existing" -lt "$total" ]; then
    echo ""
    echo "  Incomplete run: $existing / $total results in $OUTPUT_DIR"
    echo ""
    read -rp "  Resume (r) or start fresh (f)? [r/f]: " _choice
    case "$(echo "${_choice}" | tr '[:upper:]' '[:lower:]')" in
      r) echo "" ;;
      f) OUTPUT_DIR="$ROOT_DIR/results/$(date +%Y%m%d_%H%M%S)"
         echo "  Starting fresh → $OUTPUT_DIR"
         echo "" ;;
      *) echo "ERROR: enter 'r' to resume or 'f' for fresh run." >&2; exit 1 ;;
    esac
    unset _choice
  elif [ "$existing" -ge "$total" ]; then
    echo ""
    echo "  Run already complete ($existing / $total results in $OUTPUT_DIR)."
    echo "  Starting fresh → use a different --output dir or omit it for a new timestamp."
    echo ""
    OUTPUT_DIR="$ROOT_DIR/results/$(date +%Y%m%d_%H%M%S)"
  fi
fi

# ── Helpers ────────────────────────────────────────────────────────────────────

log() { echo "[$(date +%H:%M:%S)] $*"; }
die() { echo "ERROR: $*" >&2; exit 1; }

relay_ssh() { ssh -o ConnectTimeout=10 "$RELAY_SSH" "$@"; }
pub_ssh()   {
  if [ -n "$PUB_SSH" ]; then
    ssh -o ConnectTimeout=10 "$PUB_SSH" "$@"
  else
    eval "$@"
  fi
}

detect_subscriber_ip() {
  if [[ "$(uname)" == "Darwin" ]]; then
    local iface
    iface=$(route get "$RELAY_HOST_IP" 2>/dev/null | awk '/interface:/{print $2}')
    ipconfig getifaddr "$iface" 2>/dev/null \
      || die "Cannot detect local IP toward relay ($RELAY_HOST_IP). Check your network."
  else
    ip route get "$RELAY_HOST_IP" 2>/dev/null \
      | awk '{for(i=1;i<=NF;i++) if ($i=="src") {print $(i+1); exit}}' \
      || die "Cannot detect local IP toward relay ($RELAY_HOST_IP). Check your network."
  fi
}

# ── Build ──────────────────────────────────────────────────────────────────────

do_build() {
  if [ -n "$PUB_SSH" ] && [ "$PUB_SSH" = "$RELAY_SSH" ]; then
    log "Building relay + client binaries on relay host..."
    relay_ssh "source ~/.cargo/env && cd $RELAY_PROJECT && cargo build --release --bin relay --bin client 2>&1 | tail -5"
  else
    log "Building relay binary on relay host..."
    relay_ssh "source ~/.cargo/env && cd $RELAY_PROJECT && cargo build --release --bin relay 2>&1 | tail -5"
    if [ -n "$PUB_SSH" ]; then
      log "Building client binary on publisher host..."
      pub_ssh "source ~/.cargo/env && cd $PUB_PROJECT && cargo build --release --bin client 2>&1 | tail -5"
    fi
  fi

  log "Building client locally (for subscriber)..."
  cargo build --release --bin client --manifest-path "$ROOT_DIR/Cargo.toml" 2>&1 | tail -5
}

# ── Relay ──────────────────────────────────────────────────────────────────────

start_relay() {
  log "Stopping any lingering relay on relay host..."
  relay_ssh "pkill -x relay 2>/dev/null; true"
  sleep 2

  log "Starting relay on relay host (port $RELAY_PORT)..."
  relay_ssh "cd $RELAY_PROJECT && python3 - <<'PYEOF'
import subprocess, sys
p = subprocess.Popen(
    ['target/release/relay', '--port', '$RELAY_PORT'],
    stdin=open('/dev/null'), stdout=open('/tmp/moqtail-relay.log','w'),
    stderr=subprocess.STDOUT, close_fds=True, start_new_session=True)
open('/tmp/moqtail-relay.pid','w').write(str(p.pid))
print('relay PID', p.pid, flush=True)
PYEOF"
  sleep 4
}

stop_relay() {
  log "Stopping relay on relay host..."
  relay_ssh "[ -f /tmp/moqtail-relay.pid ] && \
    kill \$(cat /tmp/moqtail-relay.pid) 2>/dev/null; \
    rm -f /tmp/moqtail-relay.pid; \
    true"
}

# ── Publisher ──────────────────────────────────────────────────────────────────

start_publisher() {
  local pub_bin="$PUB_PROJECT/target/release/client"

  if [ -n "$PUB_SSH" ]; then
    if ! pub_ssh "test -x $pub_bin" 2>/dev/null; then
      die "Client binary not found on publisher host at $pub_bin — run with --build first"
    fi
    log "Stopping any lingering publisher on $PUB_SSH..."
    pub_ssh "pkill -f '[c]lient.*publish-multi' 2>/dev/null; true"
    sleep 1

    log "Starting publish-multi on $PUB_SSH (tracks=$PUB_TRACKS, groups=$PUB_GROUP_COUNT)..."
    pub_ssh "cd $PUB_PROJECT && python3 - <<'PYEOF'
import subprocess
p = subprocess.Popen(
    ['target/release/client',
     '--server', '$PUB_RELAY_URL',
     '--namespace', '$NAMESPACE',
     '--command', 'publish-multi',
     '--no-cert-validation',
     '--tracks', '$PUB_TRACKS',
     '--objects-per-group', '$PUB_OBJECTS_PER_GROUP',
     '--interval', '$PUB_INTERVAL_MS',
     '--group-count', '$PUB_GROUP_COUNT'],
    stdin=open('/dev/null'), stdout=open('$PUB_LOG','w'),
    stderr=subprocess.STDOUT, close_fds=True, start_new_session=True)
open('$PUB_PID_FILE','w').write(str(p.pid))
print('publisher PID', p.pid, flush=True)
PYEOF"
  else
    if [ ! -x "$pub_bin" ]; then
      die "Client binary not found at $pub_bin — run with --build first"
    fi
    log "Stopping any lingering local publisher..."
    pkill -f "[c]lient.*publish-multi" 2>/dev/null || true
    sleep 1

    log "Starting publish-multi locally (tracks=$PUB_TRACKS, groups=$PUB_GROUP_COUNT) → $PUB_LOG"
    "$pub_bin" \
      --server "$PUB_RELAY_URL" \
      --namespace "$NAMESPACE" \
      --command publish-multi \
      --no-cert-validation \
      --tracks "$PUB_TRACKS" \
      --objects-per-group "$PUB_OBJECTS_PER_GROUP" \
      --interval "$PUB_INTERVAL_MS" \
      --group-count "$PUB_GROUP_COUNT" \
      >"$PUB_LOG" 2>&1 &
    echo $! >"$PUB_PID_FILE"
    log "Publisher PID $(cat "$PUB_PID_FILE")"
  fi

  local warmup=$(( SWITCH_AFTER > 5 ? 5 : SWITCH_AFTER ))
  log "Waiting ${warmup}s for initial cache warm-up..."
  sleep "$warmup"
}

stop_publisher() {
  log "Stopping publisher..."
  if [ -n "$PUB_SSH" ]; then
    pub_ssh "[ -f $PUB_PID_FILE ] && kill \$(cat $PUB_PID_FILE) 2>/dev/null; rm -f $PUB_PID_FILE; true"
  else
    if [ -f "$PUB_PID_FILE" ]; then
      kill "$(cat "$PUB_PID_FILE")" 2>/dev/null || true
      rm -f "$PUB_PID_FILE"
    fi
  fi
}

# ── tc bandwidth shaping (on relay host, toward subscriber) ────────────────────

apply_tc() {
  local bw=$1 subscriber_ip=$2
  log "tc: ${bw} bps toward $subscriber_ip (mark=$TC_MARK)"
  if ! relay_ssh "sudo bash $RELAY_PROJECT/scripts/tc/set_bandwidth.sh $bw $subscriber_ip $TC_MARK" 2>/dev/null; then
    log "WARNING: tc failed (sudo not configured?) — running without bandwidth shaping"
    return 0
  fi
  sleep 3
}

clear_tc() {
  local subscriber_ip=$1
  log "tc: clearing rule (mark=$TC_MARK)..."
  relay_ssh "sudo bash $RELAY_PROJECT/scripts/tc/set_bandwidth.sh 0 $subscriber_ip $TC_MARK del 2>/dev/null; true"
  sleep 1
}

# ── Health check + auto-restart ────────────────────────────────────────────────

ensure_healthy() {
  # Check relay: try to reach the UDP port on the relay host.
  local relay_up=false
  if ssh -o ConnectTimeout=5 "$RELAY_SSH" \
      "ss -ulnp 2>/dev/null | grep -q ':$RELAY_PORT'" 2>/dev/null; then
    relay_up=true
  fi

  if ! $relay_up; then
    log "Relay is down — restarting relay and publisher..."
    start_relay
    stop_publisher
    start_publisher
    return
  fi

  # Check publisher process.
  local pub_alive=false
  if [ -n "$PUB_SSH" ]; then
    pub_ssh "[ -f $PUB_PID_FILE ] && kill -0 \$(cat $PUB_PID_FILE) 2>/dev/null" 2>/dev/null && pub_alive=true || true
  else
    { [ -f "$PUB_PID_FILE" ] && kill -0 "$(cat "$PUB_PID_FILE")" 2>/dev/null && pub_alive=true; } || true
  fi
  if ! $pub_alive; then
    log "Publisher is down — restarting publisher..."
    start_publisher
  fi
}

# ── Single experiment run ──────────────────────────────────────────────────────

print_result() {
  local outfile=$1
  python3 - "$outfile" <<'PYEOF'
import json, sys
d = json.load(open(sys.argv[1]))
parts = []
for i, s in enumerate(d.get('switches', [])):
    lat = s['switch_latency_ms']
    frz = s['freeze_ms']
    stall = s.get('stall_ms')
    metric = f"stall={stall}ms" if stall is not None else f"freeze={frz}ms"
    parts.append(f"sw{i+1}: {lat}ms ({metric})")
print("  " + "  |  ".join(parts))
PYEOF
}

run_one() {
  local method=$1 bw=$2 rep=$3 subscriber_ip=$4
  local label="${method}_${bw}bps"
  local outfile="$OUTPUT_DIR/${label}_rep${rep}.json"

  if [ -s "$outfile" ]; then
    log "SKIP: $label rep $rep (already done)"
    return 0
  fi

  log "--- $label rep $rep ---"

  if [ "$bw" -gt 0 ]; then
    apply_tc "$bw" "$subscriber_ip"
  fi

  if "$CLIENT_BIN" \
    --server "$RELAY_URL" \
    --namespace "$NAMESPACE" \
    --command switch-test \
    --no-cert-validation \
    --track-sequence "$TRACK_SEQUENCE" \
    --method "$method" \
    --switch-after "$SWITCH_AFTER" \
    --jitter-buffer-ms "$JITTER_BUFFER_MS" \
    --bandwidth-cap-bps 0 \
    --output-json "$outfile"; then
    log "Saved: $outfile"
    print_result "$outfile"
  else
    log "WARNING: subscriber exited with error for $label rep $rep"
  fi

  if [ "$bw" -gt 0 ]; then
    clear_tc "$subscriber_ip"
  fi

  sleep 2
}

# ── Main ───────────────────────────────────────────────────────────────────────

main() {
  local subscriber_ip
  subscriber_ip=$(detect_subscriber_ip)

  log "=== experiment.sh ==="
  log "Relay:          $RELAY_SSH  ($RELAY_HOST_IP:$RELAY_PORT)"
  log "Publisher:      ${PUB_SSH:-local} (publish-multi → $PUB_RELAY_URL)"
  log "Methods:        ${METHODS[*]}"
  log "Bandwidths:     ${BANDWIDTHS[*]} bps"
  log "Track sequence: $TRACK_SEQUENCE"
  log "Switch after:   ${SWITCH_AFTER}s"
  log "Jitter buffer:  ${JITTER_BUFFER_MS}ms"
  log "Reps:           $REPS"
  log "Subscriber IP:  $subscriber_ip"

  mkdir -p "$OUTPUT_DIR"
  log "Results:        $OUTPUT_DIR"

  if "$BUILD"; then
    do_build
  fi

  if ! "$SKIP_START"; then
    start_relay
    start_publisher
  fi

  trap 'log "Interrupted — cleaning up..."; clear_tc "$subscriber_ip" 2>/dev/null; stop_publisher; stop_relay' EXIT INT TERM

  local total=$(( ${#METHODS[@]} * ${#BANDWIDTHS[@]} * REPS ))
  local done_count=0
  log "Running $total experiment runs..."

  for method in "${METHODS[@]}"; do
    for bw in "${BANDWIDTHS[@]}"; do
      for rep in $(seq 1 "$REPS"); do
        ensure_healthy
        run_one "$method" "$bw" "$rep" "$subscriber_ip"
        (( done_count++ )) || true
        log "Progress: $done_count / $total"
      done
    done
  done

  log "All done. Results in $OUTPUT_DIR"

  trap - EXIT INT TERM
  stop_publisher
  stop_relay
}

main
