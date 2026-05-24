#!/usr/bin/env bash
# experiment.sh — Track switching experiment orchestrator
#
# Runs on the subscriber machine. Starts the relay (always remote) and the
# publisher (local by default, or remote if PUB_SSH is set) then iterates
# over a method × bandwidth matrix, saving one JSON file per run.
#
# Configuration is read from scripts/.env (gitignored). Copy
# scripts/.env.example to scripts/.env and fill in your values.
#
# Usage:
#   bash scripts/experiment.sh [options]
#
# Options:
#   --build              Build release binaries on relay (and publisher if remote)
#                        and the client binary locally, before running
#   --skip-start         Assume relay + publisher are already running
#   --method  <name>     Only run this method  (repeatable; default: all three)
#   --bandwidth <bps>    Only run at this bandwidth; 0 = no limit (repeatable; default: all)
#   --track-sequence <s> Comma-separated track sequence, e.g. "2,3,4,3,2"
#                        Default: "2,3,4,3,2" (up-up-down-down across 4 bitrates)
#   --switch-after <ms>  Milliseconds before triggering each switch (default: 15000)
#   --jitter-buffer-ms <ms>  Jitter buffer for realtime freeze calculation (default: 100)
#   --reps <n>           Repetitions per condition (default: 3)
#   --output <dir>       Results directory (default: results/YYYYMMDD_HHMMSS)
#   --help

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

# Required variables (must be set in .env)
: "${RELAY_SSH:?RELAY_SSH must be set in scripts/.env}"
: "${RELAY_PROJECT:?RELAY_PROJECT must be set in scripts/.env}"

# Optional variables with defaults
RELAY_PORT="${RELAY_PORT:-4433}"
PUB_SSH="${PUB_SSH:-}"           # empty = publisher runs locally (same machine as subscriber)
PUB_PROJECT="${PUB_PROJECT:-}"   # only needed when PUB_SSH is set

# Derive relay host IP from RELAY_SSH (strips "user@" prefix)
RELAY_HOST_IP="${RELAY_SSH##*@}"
RELAY_URL="https://${RELAY_HOST_IP}:${RELAY_PORT}"

# Namespace published by moqtail-pub
NAMESPACE="${NAMESPACE:-moqtail-watch-party-live}"

# ── Experiment defaults ────────────────────────────────────────────────────────

ALL_METHODS=("switch-message" "sub-update-forward" "joining-fetch")
# Bandwidth ladder (bps). 0 = unlimited baseline (no tc rule applied).
ALL_BANDWIDTHS=(0 5000000 3000000 2000000 1500000 1000000)
# Track sequence: lower index = lower bitrate (matches ffmpeg.sh ordering)
#   track 1=360p/500kbps, 2=480p/1Mbps, 3=720p/2.5Mbps, 4=1080p/4Mbps
TRACK_SEQUENCE="2,3,4,3,2"
SWITCH_AFTER=15000    # milliseconds before triggering each switch
JITTER_BUFFER_MS=40   # ms; one frame at 25fps
REPS=3
TC_MARK=1         # iptables mark; use a consistent value per subscriber (1–255)

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
JITTER_BUFFER_MS_OVERRIDE=""

usage() {
  grep '^#' "$0" | sed 's/^# \{0,1\}//' | tail -n +2
  exit 0
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --build)              BUILD=true;                              shift ;;
    --skip-start)         SKIP_START=true;                         shift ;;
    --method)             SELECTED_METHODS+=("$2");                 shift 2 ;;
    --bandwidth)          SELECTED_BANDWIDTHS+=("$2");              shift 2 ;;
    --track-sequence)     TRACK_SEQUENCE="$2";                     shift 2 ;;
    --switch-after)       SWITCH_AFTER="$2";                       shift 2 ;;
    --jitter-buffer-ms)   JITTER_BUFFER_MS_OVERRIDE="$2";          shift 2 ;;
    --reps)               REPS="$2";                               shift 2 ;;
    --output)             OUTPUT_DIR="$2";                         shift 2 ;;
    --help|-h)            usage ;;
    *) echo "Unknown option: $1" >&2; exit 1 ;;
  esac
done

[ ${#SELECTED_METHODS[@]}    -eq 0 ] && METHODS=("${ALL_METHODS[@]}")    || METHODS=("${SELECTED_METHODS[@]}")
[ ${#SELECTED_BANDWIDTHS[@]} -eq 0 ] && BANDWIDTHS=("${ALL_BANDWIDTHS[@]}") || BANDWIDTHS=("${SELECTED_BANDWIDTHS[@]}")
[ -n "$JITTER_BUFFER_MS_OVERRIDE" ] && JITTER_BUFFER_MS="$JITTER_BUFFER_MS_OVERRIDE"
[ -z "$OUTPUT_DIR" ] && OUTPUT_DIR="$ROOT_DIR/results/$(date +%Y%m%d_%H%M%S)"

# ── Helpers ────────────────────────────────────────────────────────────────────

log() { echo "[$(date +%H:%M:%S)] $*"; }
die() { echo "ERROR: $*" >&2; exit 1; }

relay_ssh() { ssh -o ConnectTimeout=10 "$RELAY_SSH" "$@"; }

pub_ssh() {
  if [ -n "$PUB_SSH" ]; then
    ssh -o ConnectTimeout=10 "$PUB_SSH" "$@"
  else
    bash -c "$*"
  fi
}

# Detect the subscriber's IP on the interface that routes toward the relay.
# Works on both macOS and Linux.
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

build_relay() {
  log "Building relay binary on relay host..."
  relay_ssh "source ~/.cargo/env && cd $RELAY_PROJECT && cargo build --release --bin relay 2>&1 | tail -5"
}

build_publisher() {
  log "Building publisher binary..."
  if [ -n "$PUB_SSH" ]; then
    ssh -o ConnectTimeout=10 "$PUB_SSH" \
      "cd $PUB_PROJECT && cargo build --release --bin moqtail-pub 2>&1 | tail -5"
  else
    cargo build --release --bin moqtail-pub --manifest-path "$ROOT_DIR/Cargo.toml" 2>&1 | tail -5
  fi
}

build_subscriber() {
  log "Building subscriber client locally..."
  cargo build --release --bin client --manifest-path "$ROOT_DIR/Cargo.toml" 2>&1 | tail -5
}

# ── Relay process management ───────────────────────────────────────────────────

start_relay() {
  log "Stopping any lingering relay on relay host..."
  relay_ssh "pkill -x relay 2>/dev/null; true"
  sleep 2

  log "Starting relay on relay host (port $RELAY_PORT)..."
  relay_ssh "cd $RELAY_PROJECT && \
    nohup target/release/relay </dev/null > /tmp/moqtail-relay.log 2>&1 & \
    echo \$! > /tmp/moqtail-relay.pid && \
    echo 'relay PID' \$(cat /tmp/moqtail-relay.pid)"
  sleep 4
}

stop_relay() {
  log "Stopping relay on relay host..."
  relay_ssh "[ -f /tmp/moqtail-relay.pid ] && \
    kill \$(cat /tmp/moqtail-relay.pid) 2>/dev/null; true"
}

# ── Publisher process management ───────────────────────────────────────────────

start_publisher() {
  log "Stopping any lingering publisher and ffmpeg..."
  pub_ssh "pkill -f 'target/release/moqtail-pub' 2>/dev/null; pkill -f ffmpeg 2>/dev/null; true"
  sleep 1

  local project="${PUB_PROJECT:-$ROOT_DIR}"
  log "Starting ffmpeg + publisher (connecting to $RELAY_URL)..."
  pub_ssh "cd $project && \
    nohup bash scripts/ffmpeg.sh --url https://${RELAY_HOST_IP}:${RELAY_PORT} --release \
      > $PUB_LOG 2>&1 & \
    echo \$! > $PUB_PID_FILE && \
    echo 'publisher PID' \$(cat $PUB_PID_FILE)"
  sleep 6  # allow ffmpeg + publisher to connect and start pushing objects
}

stop_publisher() {
  log "Stopping publisher and ffmpeg..."
  pub_ssh "[ -f $PUB_PID_FILE ] && kill \$(cat $PUB_PID_FILE) 2>/dev/null; \
           pkill -f ffmpeg 2>/dev/null; \
           rm -f $PUB_PID_FILE; \
           true"
}

# ── tc bandwidth shaping (always on relay host, toward subscriber) ─────────────

apply_tc() {
  local bw=$1 subscriber_ip=$2
  log "tc: ${bw} bps toward $subscriber_ip (mark=$TC_MARK)"
  relay_ssh "sudo bash $RELAY_PROJECT/scripts/tc/set_bandwidth.sh $bw $subscriber_ip $TC_MARK"
  sleep 3  # let QUIC probe the new rate
}

clear_tc() {
  local subscriber_ip=$1
  log "tc: clearing rule (mark=$TC_MARK)..."
  relay_ssh "sudo bash $RELAY_PROJECT/scripts/tc/set_bandwidth.sh 0 $subscriber_ip $TC_MARK del 2>/dev/null; true"
  sleep 1
}

# ── Single experiment run ──────────────────────────────────────────────────────

run_one() {
  local method=$1 bw=$2 rep=$3 subscriber_ip=$4
  local label="${method}_${bw}bps"
  local outfile="$OUTPUT_DIR/${label}_rep${rep}.json"

  log "--- $label rep $rep ---"

  if [ "$bw" -gt 0 ]; then
    apply_tc "$bw" "$subscriber_ip"
  fi

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
    || log "WARNING: subscriber exited with error for $label rep $rep"

  if [ "$bw" -gt 0 ]; then
    clear_tc "$subscriber_ip"
  fi

  sleep 2  # let the QUIC connection settle before the next run
}

# ── Main ───────────────────────────────────────────────────────────────────────

main() {
  local subscriber_ip
  subscriber_ip=$(detect_subscriber_ip)
  log "Subscriber IP toward relay: $subscriber_ip"
  log "Relay:          $RELAY_SSH  ($RELAY_HOST_IP:$RELAY_PORT)"
  log "Publisher:      ${PUB_SSH:-local}"
  log "Methods:        ${METHODS[*]}"
  log "Bandwidths:     ${BANDWIDTHS[*]} bps"
  log "Track sequence: $TRACK_SEQUENCE"
  log "Switch after:   ${SWITCH_AFTER}ms"
  log "Jitter buffer:  ${JITTER_BUFFER_MS}ms"
  log "Reps:           $REPS"

  mkdir -p "$OUTPUT_DIR"
  log "Results:    $OUTPUT_DIR"

  if "$BUILD"; then
    build_relay
    build_publisher
    build_subscriber
  fi

  if ! "$SKIP_START"; then
    start_relay
    start_publisher
  fi

  trap 'log "Interrupted — cleaning up..."; clear_tc "$subscriber_ip" 2>/dev/null; stop_publisher; stop_relay' EXIT INT TERM

  local total=$(( ${#METHODS[@]} * ${#BANDWIDTHS[@]} * REPS ))
  local done=0
  log "Running $total experiment runs..."

  for method in "${METHODS[@]}"; do
    for bw in "${BANDWIDTHS[@]}"; do
      for rep in $(seq 1 "$REPS"); do
        run_one "$method" "$bw" "$rep" "$subscriber_ip"
        (( done++ )) || true
        log "Progress: $done / $total"
      done
    done
  done

  log "All done. Results in $OUTPUT_DIR"

  trap - EXIT INT TERM
  stop_publisher
  stop_relay
}

main
