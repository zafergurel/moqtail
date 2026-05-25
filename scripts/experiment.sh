#!/usr/bin/env bash
# experiment.sh — Track switching experiment (remote relay, local publish-multi publisher)
#
# Runs on the subscriber machine. Starts the relay on a remote SSH host and a
# local publish-multi publisher (artificial bytes), then iterates over a
# method × scenario matrix, saving one JSON file per run.
#
# Configuration is read from scripts/.env (gitignored). Copy
# scripts/.env.example to scripts/.env and fill in your values.
#
# Usage:
#   bash scripts/experiment.sh [options]
#
# Options:
#   --build                    Build release binaries on relay and client locally
#   --skip-start               Skip the initial relay startup (assume it is already running)
#   --no-restart-services      Do NOT restart relay+publisher before each run (default: restart each run)
#   --method  <name>           Only run this method (repeatable; default: all three)
#   --reps <n>                 Repetitions per condition (default: 3)
#   --delays <list>            Comma-separated relative-position delays in ms (default: 0,1000,2000)
#                              0 → sync baseline with BW scenarios; N → a_ahead_N and b_ahead_N RP scenarios
#   --switch-after <ms>        Milliseconds per track before triggering the switch (default: 4400)
#   --jitter-buffer-ms <ms>    Jitter buffer for realtime freeze calculation (default: 500)
#   --objects-per-group <n>    Objects per group (default: 25, i.e. 1s GOP at 25fps)
#   --interval <ms>            Inter-object interval in ms (default: 40, i.e. 25fps)
#   --group-count <n>          Total groups to publish (default: 5000 ≈ ~83 minutes)
#   --output <dir>             Results directory (default: results/YYYYMMDD_HHMMSS)
#   --help
#
# Scenario matrix (generated from --delays; default 0,1000,2000):
#
#   delay=0 → sync relative-position + bandwidth-condition scenarios:
#     rp_a2b_sync   A→B  sync (no offset), unlimited BW
#     rp_b2a_sync   B→A  sync (no offset), unlimited BW
#     bw_a2b_4500k  A→B  4.5 Mbps  (just above track B)
#     bw_a2b_7000k  A→B  7.0 Mbps  (comfortable)
#     bw_b2a_3000k  B→A  3.0 Mbps  (B=4 Mbps exceeds link; A=2.5 Mbps fits)
#     bw_b2a_7000k  B→A  7.0 Mbps  (comfortable)
#
#   delay=N → relative-position scenarios (A→B and B→A, unlimited BW):
#     rp_a2b_aN     A→B  A ahead by N ms  (B starts N ms late)
#     rp_b2a_aN     B→A  A ahead by N ms
#     rp_a2b_bN     A→B  B ahead by N ms  (A starts N ms late)
#     rp_b2a_bN     B→A  B ahead by N ms
#
# Examples:
#   bash scripts/experiment.sh --build
#   bash scripts/experiment.sh --method switch --reps 1
#   bash scripts/experiment.sh --delays 0,1000,2000 --build

set -euo pipefail

INVOCATION="$0 $*"

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

# ── Load environment ────────────────────────────────────────────────────────────

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

PUB_SSH="${PUB_SSH:-}"
PUB_PROJECT="${PUB_PROJECT:-$ROOT_DIR}"
if [ -n "$PUB_SSH" ] && [ "$PUB_SSH" = "$RELAY_SSH" ]; then
  PUB_RELAY_URL="https://127.0.0.1:${RELAY_PORT}"
else
  PUB_RELAY_URL="$RELAY_URL"
fi

# ── Experiment defaults ─────────────────────────────────────────────────────────

ALL_METHODS=("switch" "sub-update-forward" "joining-fetch")
SWITCH_AFTER=4400
JITTER_BUFFER_MS=500
REPS=3
DELAYS="0,1000,2000"
TC_MARK=1

# Track A = track 3 (2.5 Mbps), Track B = track 4 (4 Mbps)
# payload_size B/obj at 25fps: 2.5 Mbps = 12500 B/obj, 4 Mbps = 20000 B/obj
TRACK_A="3"
TRACK_B="4"
TRACK_A_PAYLOAD=12500
TRACK_B_PAYLOAD=20000
TRACK_P_RATIO="0.25"

PUB_OBJECTS_PER_GROUP=25
PUB_INTERVAL_MS=40
PUB_GROUP_COUNT=5000

# PUB_GROUPS and TOTAL_SCENARIOS are built dynamically after flag parsing.

# ── Paths ───────────────────────────────────────────────────────────────────────

CLIENT_BIN="$ROOT_DIR/target/release/client"
PUB_LOG="/tmp/moqtail-pub.log"
PUB_PID_FILE="/tmp/moqtail-pub.pid"
# PUB_TRACKS is set dynamically per publisher group
PUB_TRACKS=""

# ── Flag overrides ──────────────────────────────────────────────────────────────

BUILD=false
SKIP_START=false
RESTART_SERVICES=true
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
    --no-restart-services) RESTART_SERVICES=false;              shift ;;
    --method)              SELECTED_METHODS+=("$2");             shift 2 ;;
    --reps)              REPS="$2";                           shift 2 ;;
    --delays)            DELAYS="$2";                         shift 2 ;;
    --switch-after)          SWITCH_AFTER="$2";                shift 2 ;;
    --jitter-buffer-ms)      JITTER_BUFFER_MS="$2";           shift 2 ;;
    --objects-per-group) PUB_OBJECTS_PER_GROUP="$2";          shift 2 ;;
    --interval)          PUB_INTERVAL_MS="$2";                shift 2 ;;
    --group-count)       PUB_GROUP_COUNT="$2";                shift 2 ;;
    --output)            OUTPUT_DIR="$2";                     shift 2 ;;
    --help|-h)           usage ;;
    *) echo "Unknown option: $1" >&2; exit 1 ;;
  esac
done

[ ${#SELECTED_METHODS[@]} -eq 0 ] && METHODS=("${ALL_METHODS[@]}") || METHODS=("${SELECTED_METHODS[@]}")
[ -z "$OUTPUT_DIR" ] && OUTPUT_DIR="$ROOT_DIR/results/$(date +%Y%m%d_%H%M%S)"

# ── Build scenario matrix from --delays ─────────────────────────────────────────
# Each PUB_GROUPS entry: "pub_label|delay_a_ms|delay_b_ms|label:sequence:bw_bps|..."

declare -a PUB_GROUPS=()
IFS=',' read -ra _delay_list <<< "$DELAYS"
for _d in "${_delay_list[@]}"; do
  if [ "$_d" -eq 0 ]; then
    PUB_GROUPS+=("sync|0|0|rp_a2b_sync:3,4:0|rp_b2a_sync:4,3:0|bw_a2b_4500k:3,4:4500000|bw_a2b_7000k:3,4:7000000|bw_b2a_3000k:4,3:3000000|bw_b2a_7000k:4,3:7000000")
  else
    PUB_GROUPS+=("a_ahead_${_d}|0|${_d}|rp_a2b_a${_d}:3,4:0|rp_b2a_a${_d}:4,3:0")
    PUB_GROUPS+=("b_ahead_${_d}|${_d}|0|rp_a2b_b${_d}:3,4:0|rp_b2a_b${_d}:4,3:0")
  fi
done
unset _delay_list _d

TOTAL_SCENARIOS=0
for _g in "${PUB_GROUPS[@]}"; do
  IFS='|' read -ra _p <<< "$_g"
  (( TOTAL_SCENARIOS += ${#_p[@]} - 3 )) || true
done
unset _g _p

# ── Resume / fresh prompt ───────────────────────────────────────────────────────

total=$(( TOTAL_SCENARIOS * ${#METHODS[@]} * REPS ))

if [ -d "$OUTPUT_DIR" ]; then
  existing=$(find "$OUTPUT_DIR" -maxdepth 1 -name '*.json' | wc -l)
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

# ── Helpers ─────────────────────────────────────────────────────────────────────

log() { echo "[$(date +%H:%M:%S)] $*"; }
die() { echo "ERROR: $*" >&2; exit 1; }

write_metadata() {
  local meta_file="$OUTPUT_DIR/experiment-metadata.json"
  local methods_json
  methods_json=$(python3 -c "import json,sys; print(json.dumps(sys.argv[1:]))" "${METHODS[@]}")

  # Build scenario list: one line per scenario (label|sequence|bw_bps|delay_a_ms|delay_b_ms)
  local tmp_scen
  tmp_scen=$(mktemp)
  local group_entry
  for group_entry in "${PUB_GROUPS[@]}"; do
    IFS='|' read -ra _gp <<< "$group_entry"
    local _da="${_gp[1]}" _db="${_gp[2]}"
    local _i
    for (( _i=3; _i<${#_gp[@]}; _i++ )); do
      local _lbl _seq _bw
      IFS=':' read -r _lbl _seq _bw <<< "${_gp[$_i]}"
      printf '%s|%s|%s|%s|%s\n' "$_lbl" "$_seq" "$_bw" "$_da" "$_db" >> "$tmp_scen"
    done
  done

  python3 - "$tmp_scen" <<PYEOF
import json, sys
from pathlib import Path
scenarios = []
for line in open(sys.argv[1]).read().strip().splitlines():
    lbl, seq, bw, da, db = line.split('|')
    scenarios.append({"label": lbl, "sequence": seq,
                       "bw_bps": int(bw), "delay_a_ms": int(da), "delay_b_ms": int(db)})
meta = {
    "source":           "experiment.sh",
    "timestamp":        "$(date +%Y%m%d_%H%M%S)",
    "relay_host":       "$RELAY_SSH",
    "relay_host_ip":    "$RELAY_HOST_IP",
    "relay_port":       $RELAY_PORT,
    "subscriber_ip":    "$subscriber_ip",
    "delays":           "$DELAYS",
    "switch_after_ms":  $SWITCH_AFTER,
    "jitter_buffer_ms": $JITTER_BUFFER_MS,
    "reps":             $REPS,
    "methods":          $methods_json,
    "track_a":          "$TRACK_A",
    "track_b":          "$TRACK_B",
    "track_a_payload":  $TRACK_A_PAYLOAD,
    "track_b_payload":  $TRACK_B_PAYLOAD,
    "objects_per_group": $PUB_OBJECTS_PER_GROUP,
    "interval_ms":      $PUB_INTERVAL_MS,
    "group_count":      $PUB_GROUP_COUNT,
    "scenarios":        scenarios,
    "command":          "$INVOCATION",
}
Path("$meta_file").write_text(json.dumps(meta, indent=2) + "\n")
PYEOF
  rm -f "$tmp_scen"
  log "Metadata: $meta_file"
}

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

# ── Build ───────────────────────────────────────────────────────────────────────

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

# ── Relay ───────────────────────────────────────────────────────────────────────

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

# ── Publisher ───────────────────────────────────────────────────────────────────

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

  log "Waiting $(( SWITCH_AFTER / 1000 ))s for initial cache warm-up..."
  sleep $(( SWITCH_AFTER / 1000 ))
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

# ── tc bandwidth shaping (on relay host, toward subscriber) ─────────────────────

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

# ── Health check + auto-restart ─────────────────────────────────────────────────

ensure_healthy() {
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

# ── Single experiment run ───────────────────────────────────────────────────────

print_result() {
  local outfile=$1
  python3 - "$outfile" <<'PYEOF'
import json, sys
d = json.load(open(sys.argv[1]))
parts = []
for i, s in enumerate(d.get('switches', [])):
    lat = s.get('switch_delay_ms')
    stall = s.get('stall_ms')
    parts.append(f"sw{i+1}: {lat}ms (stall={stall}ms)")
print("  " + "  |  ".join(parts))
PYEOF
}

run_one() {
  local method=$1 scenario=$2 sequence=$3 bw=$4 rep=$5 subscriber_ip=$6
  local label="${method}_${scenario}"
  local outfile="$OUTPUT_DIR/${label}_rep${rep}.json"

  if [ -s "$outfile" ]; then
    log "SKIP: $label rep $rep (already done)"
    return 0
  fi

  if "$RESTART_SERVICES"; then
    stop_publisher
    stop_relay
    start_relay
    start_publisher
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
    --track-sequence "$sequence" \
    --method "$method" \
    --switch-after "$SWITCH_AFTER" \
    --jitter-buffer-ms "$JITTER_BUFFER_MS" \
    --bandwidth-cap-bps "$bw" \
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

# ── Publisher group runner ──────────────────────────────────────────────────────
# Args: pub_label delay_a_ms delay_b_ms scenario_spec...
# scenario_spec format: "label:sequence:bw_bps"

run_pub_group() {
  local pub_label=$1
  local delay_a=$2
  local delay_b=$3
  shift 3

  log ""
  log "=== Publisher group: $pub_label (delay_A=${delay_a}ms, delay_B=${delay_b}ms) ==="

  PUB_TRACKS="${TRACK_A}:${TRACK_A_PAYLOAD}:${TRACK_P_RATIO}:${delay_a},${TRACK_B}:${TRACK_B_PAYLOAD}:${TRACK_P_RATIO}:${delay_b}"

  if ! "$RESTART_SERVICES"; then
    # Services shared across runs — restart publisher once per group (delay config may have changed).
    stop_publisher
    start_publisher
  fi

  local scenario_spec scenario_label sequence bw
  for scenario_spec in "$@"; do
    IFS=':' read -r scenario_label sequence bw <<< "$scenario_spec"
    for method in "${METHODS[@]}"; do
      for rep in $(seq 1 "$REPS"); do
        if ! "$RESTART_SERVICES"; then
          ensure_healthy
        fi
        run_one "$method" "$scenario_label" "$sequence" "$bw" "$rep" "$subscriber_ip"
        (( done_count++ )) || true
        log "Progress: $done_count / $total"
      done
    done
  done
}

# ── Main ────────────────────────────────────────────────────────────────────────

main() {
  subscriber_ip=$(detect_subscriber_ip)
  done_count=0

  log "=== experiment.sh ==="
  log "Relay:            $RELAY_SSH  ($RELAY_HOST_IP:$RELAY_PORT)"
  log "Publisher:        ${PUB_SSH:-local}"
  log "Methods:          ${METHODS[*]}"
  log "Delays:           $DELAYS"
  log "Switch after:     ${SWITCH_AFTER}ms"
  log "Jitter buffer:    ${JITTER_BUFFER_MS}ms"
  log "Reps:             $REPS"
  log "Restart services: $RESTART_SERVICES"
  log "Subscriber IP:    $subscriber_ip"
  log "Total runs:       $total"

  mkdir -p "$OUTPUT_DIR"
  log "Results:       $OUTPUT_DIR"
  write_metadata

  if "$BUILD"; then
    do_build
  fi

  if ! "$SKIP_START" && ! "$RESTART_SERVICES"; then
    # Services shared across runs — start relay once here.
    start_relay
  fi

  trap 'log "Interrupted — cleaning up..."; clear_tc "$subscriber_ip" 2>/dev/null; stop_publisher; stop_relay' EXIT INT TERM

  for group_entry in "${PUB_GROUPS[@]}"; do
    IFS='|' read -ra parts <<< "$group_entry"
    local_pub_label="${parts[0]}"
    local_delay_a="${parts[1]}"
    local_delay_b="${parts[2]}"
    # Remaining parts are scenario specs
    local_scenarios=("${parts[@]:3}")
    run_pub_group "$local_pub_label" "$local_delay_a" "$local_delay_b" "${local_scenarios[@]}"
  done

  log ""
  log "All done. Results in $OUTPUT_DIR"

  trap - EXIT INT TERM
  stop_publisher
  stop_relay

  local summary_file="$OUTPUT_DIR/results.md"
  python3 "$ROOT_DIR/results/summarize.py" "$OUTPUT_DIR" > "$summary_file" 2>&1 \
    && log "Summary: $summary_file" \
    || log "WARNING: summarize.py failed"
}

main
