# Track Switching Experiment

Empirical comparison of three subscriber-initiated track-switching methods in MOQ Transport, for the paper _"Zero Gap, Zero Waste: Subscriber-Initiated, Relay-Executed Track Switching in MOQ Transport"_.

---

## Contents

1. [Build](#1-build)
2. [Local testing](#2-local-testing)
3. [Multi-machine testing](#3-multi-machine-testing)
4. [Running a single switch manually](#4-running-a-single-switch-manually)
5. [How the system works](#5-how-the-system-works)
6. [How metrics are computed](#6-how-metrics-are-computed)
7. [Publisher delay and phase offset](#7-publisher-delay-and-phase-offset)
8. [Analyzing results](#8-analyzing-results)
9. [Reference](#9-reference)

---

## 1. Build

All commands run from the workspace root (directory containing `Cargo.toml`).

```bash
# Debug build — fast compile, slower binary; fine for development
cargo build --bin relay --bin client

# Release build — required for experiments; use this for all timing measurements
cargo build --release --bin relay --bin client
```

Binaries land in `target/release/` (or `target/debug/`).

The relay needs TLS certificates. Self-signed ones are included:

```
apps/relay/cert/cert.pem
apps/relay/cert/key.pem
```

Run the relay from the workspace root so the default relative paths resolve:

```bash
./target/release/relay
# Listening on https://localhost:4433
```

---

## 2. Local testing

Local tests use `publish-multi` — a synthetic publisher that emits artificial bytes at realistic bitrates with a realistic GOP structure (I-frame + P-frames). No real video, no SSH, no `tc`.

### 2.1 Quickest smoke test

Three terminal windows, all from the workspace root:

**Terminal 1 — relay**

```bash
./target/release/relay
```

**Terminal 2 — publisher**

```bash
./target/release/client \
  --server https://127.0.0.1:4433 \
  --namespace moqtail-experiment \
  --command publish-multi \
  --no-cert-validation \
  --tracks "3:12500:0.25:0,4:20000:0.25:0"
```

**Terminal 3 — one switch test**

```bash
./target/release/client \
  --server https://127.0.0.1:4433 \
  --namespace moqtail-experiment \
  --command switch-test \
  --no-cert-validation \
  --method switch-message \
  --track-sequence "3,4" \
  --switch-after 5 \
  --jitter-buffer-ms 40 \
  --output-json /tmp/result.json

cat /tmp/result.json
```

### 2.2 Using `local_test.sh`

`scripts/local_test.sh` handles everything: starts relay and publisher, iterates over methods and repetitions, saves one JSON per run.

```bash
# Full run: build release binaries, 3 methods × 3 reps
bash scripts/local_test.sh --build

# Custom tracks and scenario label
bash scripts/local_test.sh \
  --tracks "3:12500:0.25:0,4:20000:0.25:0" \
  --track-sequence "3,4" \
  --switch-after 5 \
  --jitter-buffer-ms 40 \
  --scenario "sync" \
  --reps 3

# Run 3 scenario groups (sync, A-ahead 500ms, B-ahead 500ms):
#   Group 1 — sync
bash scripts/local_test.sh \
  --tracks "3:12500:0.25:0,4:20000:0.25:0" \
  --track-sequence "3,4" --switch-after 5 --jitter-buffer-ms 500 \
  --scenario "sync" --reps 3 --output results/local_test

#   Group 2 — A ahead 500ms (B delayed)
bash scripts/local_test.sh \
  --tracks "3:12500:0.25:0,4:20000:0.25:500" \
  --track-sequence "3,4" --switch-after 5 --jitter-buffer-ms 500 \
  --scenario "a_ahead_500" --reps 3 --output results/local_test \
  --restart-pub

#   Group 3 — B ahead 500ms (A delayed)
bash scripts/local_test.sh \
  --tracks "3:12500:0.25:500,4:20000:0.25:0" \
  --track-sequence "3,4" --switch-after 5 --jitter-buffer-ms 500 \
  --scenario "b_ahead_500" --reps 3 --output results/local_test \
  --restart-pub
```

Results land in the `--output` directory as `{method}_{scenario}_rep{n}.json`.

### 2.3 `local_test.sh` flags

```
--build                 Build release binaries before running
--skip-start            Skip relay and publisher startup (both already running)
--restart-pub           Restart publisher only; relay stays running
--method <name>         Only run this method (repeatable; default: all three)
--track-sequence <s>    Comma-separated track list e.g. "3,4"
--switch-after <secs>   Seconds per track before triggering switch  [15]
--jitter-buffer-ms <ms> Jitter buffer for stall calculation         [40]
--reps <n>              Repetitions per method                      [3]
--tracks <spec>         Track specs for publish-multi
--scenario <name>       Scenario label embedded in output filenames [local]
--output <dir>          Results directory
```

### 2.4 Publisher frame sizes

`publish-multi` models I-frame / P-frame size distribution. Each track spec is `"name:avg_bytes"`, `"name:avg_bytes:p_ratio"`, or `"name:avg_bytes:p_ratio:delay_ms"`. `p_ratio` is the P-frame size as a fraction of the I-frame (default `0.25`, i.e. I-frame is 4× a P-frame). `delay_ms` delays the start of object output for that track (see [§7](#7-publisher-delay-and-phase-offset)).

With `N=25` objects/group and `p_ratio=0.25`:

| Track | avg B/obj | I-frame | P-frame |
| ----- | --------- | ------- | ------- |
| `"1"` | 2 500     | 8 929   | 2 232   |
| `"2"` | 5 000     | 17 857  | 4 464   |
| `"3"` | 12 500    | 44 643  | 11 161  |
| `"4"` | 20 000    | 71 429  | 17 857  |

---

## 3. Multi-machine testing

Uses a relay on a remote Linux host; `tc` bandwidth shaping applied on the relay's egress toward the subscriber.

### 3.1 One-time relay host setup

```bash
# SSH key access
ssh-copy-id zafer@<relay-ip>

# Passwordless sudo for tc on the relay (append via visudo):
#   zafer ALL=(root) NOPASSWD: /usr/sbin/tc, /usr/sbin/iptables

# Set the network interface for tc shaping
ssh zafer@<relay-ip> \
  "echo 'INTERFACE=eth0' > ~/projects/moqtail/scripts/tc/.env"

# Build relay binary on the relay host
ssh zafer@<relay-ip> \
  "cd ~/projects/moqtail && cargo build --release --bin relay"

# Start the relay
ssh zafer@<relay-ip> \
  "cd ~/projects/moqtail && \
   nohup ./target/release/relay > /tmp/moqtail-relay.log 2>&1 &"
```

### 3.2 Local setup

```bash
cp scripts/.env.example scripts/.env
# Edit scripts/.env: set RELAY_SSH and RELAY_PROJECT at minimum

cargo build --release --bin client
```

Minimal `scripts/.env`:

```bash
RELAY_SSH="zafer@192.168.1.22"
RELAY_PROJECT="/home/zafer/projects/moqtail"
# RELAY_PORT=4433
# PUB_SSH=""      # empty = publisher runs locally
# PUB_PROJECT=""
# NAMESPACE="moqtail-experiment"
```

### 3.3 Running the full experiment matrix

```bash
# Full run: build, start, 144 runs (16 scenarios × 3 methods × 3 reps)
bash scripts/experiment.sh --build --output results/paper_run_02

# Smoke test: relay already running, one method, 1 rep
bash scripts/experiment.sh --skip-start --method switch-message --reps 1

# Resume a partial run (skips existing non-empty files)
bash scripts/experiment.sh --output results/paper_run_02
```

### 3.4 Manual tc bandwidth shaping

```bash
# On the relay host — limit outbound UDP to subscriber to 4.5 Mbps
ssh zafer@<relay-ip> \
  "sudo bash scripts/tc/set_bandwidth.sh 4500000 <subscriber-ip> 1"

# Remove the rule
ssh zafer@<relay-ip> \
  "sudo bash scripts/tc/set_bandwidth.sh 0 <subscriber-ip> 1 del"

# Watch tc live
ssh zafer@<relay-ip> "watch /sbin/tc -s -d class show dev eth0"
```

---

## 4. Running a single switch manually

```bash
# Start relay and publisher first, then:

./target/release/client \
  --server https://127.0.0.1:4433 \
  --namespace moqtail-experiment \
  --command switch-test \
  --no-cert-validation \
  --track-sequence "3,4" \
  --method switch-message \
  --switch-after 5 \
  --jitter-buffer-ms 40 \
  --bandwidth-cap-bps 0 \
  --output-json /tmp/result.json
```

With `RUST_LOG=debug` for verbose output:

```bash
RUST_LOG=debug ./target/release/client \
  --server https://127.0.0.1:4433 \
  --namespace moqtail-experiment \
  --command switch-test \
  --no-cert-validation \
  --method joining-fetch \
  --track-sequence "3,4" \
  --switch-after 5
```

---

## 5. How the system works

### 5.1 Object flow

```
PUBLISHER (publish-multi)          RELAY                  SUBSCRIBER (switch-test)
──────────────────────────────────────────────────────────────────────────────────

 Track A  ─QUIC unistream──►  cache[A]  ─QUIC unistream──►  receiver_task()
 Track B  ─QUIC unistream──►  cache[B]  ─QUIC unistream──►       │
                                                                   │ ObjectEvent{
                               control stream                      │   track_alias,
 ◄────────────── SUBSCRIBE A ──────────────────────────────────────┤   group, object,
 ──────────────► SubscribeOk ──────────────────────────────────────►   received_at }
                                                                   │
                               [switch_after secs]                 │
 ◄─ SWITCH/REQUEST_UPDATE ────────────────────────────────────────►│
 ──────────────► SubscribeOk B ────────────────────────────────────►
                   relay executes
                   at next B boundary
 ──────────────► B objects ─────────────────────────────────────────►
```

Each track is published as a sequence of **groups** (one GoP = 25 objects × 40 ms = 1 s). Object 0 of every group is the I-frame; objects 1–24 are P-frames. The relay caches objects and forwards them to each subscriber based on subscription state (forward=true/false).

### 5.2 The three switching methods

**Method 1 — SWITCH message** (1 control message)

```
Subscriber                           Relay
    │                                  │
    │──── SWITCH(A→B) ────────────────►│  relay queues: "at B's next group boundary,
    │                                  │   stop A, start B"
    │◄─── SubscribeOk(B) ─────────────│
    │◄═══ B objects (from group N) ═══│  (first object is always object_id=0)
```

The relay stops forwarding A almost immediately upon receiving SWITCH and begins B at B's next group boundary. A few trailing A objects may already be in-flight.

**Method 2 — Sub Update Forward** (3 control messages)

```
Subscriber                           Relay
    │──── SUBSCRIBE B (forward=false)─►│  relay caches B but holds delivery
    │◄─── SubscribeOk(B) ─────────────│
    │                                  │
    │  [switch_after seconds of A]     │
    │                                  │
    │──── REQUEST_UPDATE(B, fwd=true)─►│  relay enables B delivery
    │◄═══ A objects continue ─────════│  (trailing A while waiting for B's boundary)
    │◄═══ B objects start at object=0 ═│  (relay waits for B's next group start)
    │──── REQUEST_UPDATE(A, fwd=false)►│
```

The relay delivers B starting from B's next group boundary after the REQUEST_UPDATE. The subscriber tears down A once the first live B group boundary arrives.

**Method 3 — Joining Fetch** (3–4 control messages)

```
Subscriber                                 Relay
    │──── SUBSCRIBE B (prio=200, fwd=true) ──►│
    │◄─── SubscribeOk(B) + LargestObject ─────│  relay reports B's current position
    │                                          │
    │  [if LargestObject.object > 0]           │
    │──── FETCH(joining_start=0, type=Relative)►│  fetch I-frame … LargestObject
    │◄─── FetchOk ─────────────────────────────│
    │◄═══ fetch objects (I-frame … LargestObject) ══│
    │                                          │
    │◄═══ live B from next group boundary ════│  (first object_id=0 is first decodable)
    │──── REQUEST_UPDATE(A, fwd=false) ───────►│  stop A
    │──── REQUEST_UPDATE(B, prio=128) ────────►│  raise B to normal priority
```

The relay injects the track's current `LargestObject` position into the SubscribeOk. If B is mid-group (`LargestObject.object > 0`), the client issues a Relative Joining Fetch with `joining_start=0`, asking the relay to deliver all objects in the current group from the I-frame through LargestObject. The live subscription picks up from LargestObject+1 onward. If B happens to be at a group boundary (`LargestObject.object == 0`), the fetch is skipped and the live subscription delivers the I-frame directly (3 control messages instead of 4). Fetch objects are counted as `redundant_bytes`.

**No TRACK_STATUS round trip needed:** The relay already knows B's position when it processes the SUBSCRIBE (it stores `largest_location` per track) and includes it in SubscribeOk. The client gets the information it needs in the same round trip as the subscription itself.

---

## 6. How metrics are computed

### 6.1 Timing definitions

```
                          switch_decision (t=0)
                               │
Subscriber                     │           t_last_a        t_first_b
receives:   ──A──A──A──A──A──A─┼─A─A─A─A──●───────────────●══B══B══B══►
                                │           │               │
                                │           │◄── gap ──────►│
                                │           │  (includes    │
                                │◄─────────────────────────►│
                                     switch_delay_ms
```

| Field             | Formula                         | What it measures                                                                                                                                                  |
| ----------------- | ------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `switch_delay_ms` | `t_first_b − t_decision`        | Wall-clock delay: how long until first B **I-frame** arrived after the switch decision. Inherently includes any RTTs (control-message acks, fetch round trips).   |
| `delivery_gap_ms` | `t_first_b − (t_last_a + 40ms)` | Playout gap: time between when the next frame was expected (after last A) and when first B actually arrived; negative = B arrived before A's slot ended (overlap) |
| `stall_ms`        | see §6.2                        | Estimated viewer freeze due to missed I-frame deadline                                                                                                            |

**Important**: `t_last_a` is updated for every trailing A object that arrives after the decision. It is the receive time of the _last_ A object, not the decision time. This is why `delivery_gap_ms` ≠ `switch_delay_ms`.

### 6.2 Stall formula

```
stall_ms = 0                    if delivery_gap ≤ jitter_buffer
         = delivery_gap − JB    if delivery_gap > jitter_buffer
```

If the I-frame arrived within the jitter budget the player absorbs it silently (no freeze). If it arrived late, the player was frozen for exactly `delivery_gap − JB` ms waiting for the I-frame — no more, no less. The P-frames of the GoP all follow the I-frame in order, so the decoder resumes decoding as soon as the I-frame arrives.

### 6.3 Relationship between switch_delay and stall

These two metrics measure different things and can differ significantly:

- `switch_delay` = `t_first_b − t_decision` — wall-clock time from decision to first B object
- `stall` = `max(0, delivery_gap − JB)` — how long the player actually froze

`stall` is typically **smaller** than `switch_delay` because trailing A objects advance `t_last_a` (shrinking the delivery gap relative to the decision time) and the jitter budget absorbs part of the remaining gap.

Example — `a_ahead_500` scenario, `switch-message`, `JB=500ms`:

```
t=0ms        t=140ms                          t=682ms
  │              │                               │
  │ decision     │ last A object arrives         │ first B object arrives
  │              │                               │
──┼──A──A──A──A──●                               ●══B══B══...
  │              │◄────── delivery_gap ─────────►│
  │              │   682ms - 140ms - 40ms = 502ms│
  │◄──────────────────────────────────────────── │
        switch_delay = 682ms
```

- `switch_delay` = 682ms — "B arrived 682ms after the decision."
- `delivery_gap` = 502ms — "B arrived 502ms after the expected next-frame slot."
- `stall` = 502 − 500 = **2ms** — "the I-frame was 2ms late; the player froze for 2ms."

**In summary**: `switch_delay` answers "how long did the switch take?" `stall` answers "how long did the viewer's player actually freeze?"

### 6.4 Metrics per method (expected behavior)

| Method             | delivery_gap                                                             | stall                               | AETR        | Notes                                                                                                                                    |
| ------------------ | ------------------------------------------------------------------------ | ----------------------------------- | ----------- | ---------------------------------------------------------------------------------------------------------------------------------------- |
| SWITCH message     | positive (waits for next boundary)                                       | 0 if gap ≤ JB; gap − JB if gap > JB | Low         | Gap = time from last A to B's first boundary                                                                                             |
| Sub Update Forward | ≤ 0 (slight overlap)                                                     | 0                                   | Medium–High | Pre-subscribed B starts at its next boundary; A overlaps; AETR varies with group phase at switch time                                    |
| Joining Fetch      | null when fetch is instant (no trailing A before I-frame); ≤ 0 otherwise | null / 0                            | Low–Medium  | Fetch covers partial group from relay cache; delivery_gap and stall are null when the fetch I-frame arrives before any trailing A object |

---

## 7. Publisher delay and phase offset

### 7.1 The `delay_ms` field

`publish-multi` accepts a 4th field in the track spec: `"name:avg_bytes:p_ratio:delay_ms"`. This delays the start of object output for that track while keeping the PUBLISH header exchange immediate. It creates a controlled intra-GoP phase offset between tracks.

```
delay_B = 0   (sync):
  Publisher t:  0ms       1000ms      2000ms
  Track A:      |G0───────|G1─────────|G2────...
  Track B:      |G0───────|G1─────────|G2────...
                 group boundaries are aligned

delay_B = 500ms (A is 500ms ahead of B):
  Publisher t:  0ms  500ms  1000ms     1500ms   2000ms
  Track A:      |G0─────────|G1─────────|G2────...
  Track B:           |G0─────────|G1─────────|G2────...
                      B is 500ms behind A at all times
```

### 7.2 Phase at the switch point

With `switch_after=5s` and a `5s` publisher warm-up, the switch happens when the publisher has been running ~10 s. The phase of each track at the switch point:

```
delay_A=0, delay_B=500ms (A ahead scenario, "rp_a2b_a500"):

  Publisher time at switch ≈ 10 000ms:
    Track A: running 10 000ms → group 10, object 0  (at boundary!)
    Track B: running  9 500ms → group 9,  object 12 (500ms into group)

  B's next group boundary: 500ms away
                              ↑
              This is the gap switch-message must wait for
```

```
delay_A=500ms, delay_B=0 (B ahead scenario, "rp_a2b_b500"):

  Publisher time at switch ≈ 10 000ms:
    Track A: running  9 500ms → group 9, object 12 (500ms into group)
    Track B: running 10 000ms → group 10, object 0 (at boundary!)

  B's next group boundary: 0ms away (or 1000ms to the NEXT one)
  B's most-recent boundary was just now → switch-message fires almost instantly
  and delivery_gap is negative (B's data was ~500ms pre-buffered at the relay)
```

### 7.3 How delay affects each method

| Scenario  | Method             | Expected behavior                                                                               |
| --------- | ------------------ | ----------------------------------------------------------------------------------------------- |
| A ahead δ | SWITCH message     | Waits δ ms for B's boundary → gap ≈ δ; stall occurs when δ > JB                                 |
| A ahead δ | Sub Update Forward | Waits for B's next boundary regardless of δ; gap always ≤ 0 (pre-buffered)                      |
| A ahead δ | Joining Fetch      | Fetch fills the partial group; gap = null; switch_delay ≈ 0ms (I-frame served from relay cache) |
| B ahead δ | SWITCH message     | B's boundary just passed → B data cached → gap ≈ −δ (negative, no stall)                        |
| B ahead δ | Sub Update Forward | Same as above: gap ≤ 0                                                                          |
| B ahead δ | Joining Fetch      | Fetch fills partial group; gap = null                                                           |

---

## 8. Analyzing results

### 8.1 Summarize all scenarios

```bash
python3 results/summarize.py results/paper_run_02
```

Prints mean switching delay, stall, and AETR per method × scenario.

### 8.2 Quick Python stats for a local test directory

```bash
python3 - <<'EOF'
import json, glob, statistics
from pathlib import Path

d = Path("results/local_test_20260523_005009")
METHODS   = ["switch-message", "sub-update-forward", "joining-fetch"]
SCENARIOS = ["sync", "a_ahead_500", "b_ahead_500"]

def avg(vals):
    clean = [v for v in vals if v is not None]
    return round(statistics.mean(clean)) if clean else "-"

print(f"\n{'Scenario':<16} {'Method':<22} {'Dly(ms)':>8} {'Gap(ms)':>8} {'Stl(ms)':>8} {'AETR':>8}")
print("-" * 74)
for scen in SCENARIOS:
    for m in METHODS:
        files = sorted(d.glob(f"{m}_{scen}_rep*.json"))
        if not files:
            continue
        runs = [json.loads(f.read_text()) for f in files]
        dlys = [r["switches"][0].get("switch_delay_ms") for r in runs if r.get("switches")]
        gaps = [r["switches"][0].get("delivery_gap_ms")   for r in runs if r.get("switches")]
        stls = [r["switches"][0].get("stall_ms")          for r in runs if r.get("switches")]
        aets = [r.get("aetr") for r in runs]
        print(f"{scen:<16} {m:<22} {str(avg(dlys)):>8} {str(avg(gaps)):>8} {str(avg(stls)):>8} {str(round(statistics.mean([v for v in aets if v is not None]),4) if any(v is not None for v in aets) else '-'):>8}")
    print()
EOF
```

### 8.3 Inspect a single file

```bash
cat results/local_test_20260523_005009/switch-message_sync_rep1.json | jq .
jq '.switches[]' results/**/*.json
```

### 8.4 Extract one metric across all files

```bash
# switching delay from every event
jq -r '.switches[].switch_delay_ms' results/local_*/*.json

# method + aetr, one line per file
jq -r '[.method, (.aetr | tostring)] | join("\t")' results/local_*/*.json
```

### 8.5 Per-method summary with jq

```bash
for method in switch-message sub-update-forward joining-fetch; do
  echo "── $method ──────────────────────────"
  jq -r '.switches[] | [
      (.switch_delay_ms // "null"),
      (.delivery_gap_ms   // "null"),
      (.stall_ms          // "null"),
      (.aetr * 100),
      (if .group_boundary_aligned then 1 else 0 end)
    ] | map(tostring) | join("\t")' \
    results/local_*/${method}_*.json \
  | awk 'BEGIN{OFS="\t"}
         {lat+=$1; gap+=$2; stl+=$3; aetr+=$4; gba+=$5; n++}
         END{printf "  n=%-3d  dly=%.0fms  gap=%.0fms  stall=%.0fms  AETR=%.2f%%  gba=%.0f%%\n",
             n, lat/n, gap/n, stl/n, aetr/n, gba/n*100}'
done
```

### 8.6 Collect all rows as TSV (for spreadsheet / R / Python)

```bash
echo -e "method\tbandwidth_cap_bps\trep\ttrack_from\ttrack_to\tswitch_delay_ms\tdelivery_gap_ms\tstall_ms\taetr\tgroup_boundary_aligned\tcontrol_messages\tredundant_bytes\ttrailing_a_bytes"

for f in results/**/*.json; do
  method=$(jq -r '.method' "$f")
  bw=$(jq -r '.bandwidth_cap_bps' "$f")
  rep=$(basename "$f" | grep -oE 'rep[0-9]+' | grep -oE '[0-9]+')
  jq -r --arg m "$method" --arg b "$bw" --arg r "$rep" '
    .switches[] |
    [$m, $b, $r,
     .track_from, .track_to,
     (.switch_delay_ms | tostring),
     (.delivery_gap_ms   | tostring),
     (.stall_ms          | tostring),
     (.aetr              | tostring),
     (.group_boundary_aligned | tostring),
     (.control_messages  | tostring),
     (.redundant_bytes   | tostring),
     (.trailing_a_bytes  | tostring)
    ] | join("\t")' "$f"
done
```

---

## 9. Reference

### Track layout

Lower track number = lower bitrate (video-only, no audio).

| MOQ track | Resolution | Avg bitrate | avg B/obj (25fps) | I-frame  | P-frame  |
| --------- | ---------- | ----------- | ----------------- | -------- | -------- |
| `"1"`     | 640×360    | 500 kbps    | 2 500 B           | 8 929 B  | 2 232 B  |
| `"2"`     | 854×480    | 1 Mbps      | 5 000 B           | 17 857 B | 4 464 B  |
| `"3"`     | 1280×720   | 2.5 Mbps    | 12 500 B          | 44 643 B | 11 161 B |
| `"4"`     | 1920×1080  | 4 Mbps      | 20 000 B          | 71 429 B | 17 857 B |

Primary test pair: **track 3 → track 4** (A→B upswitch) and **track 4 → track 3** (B→A downswitch).

I/P sizes computed with `p_ratio=0.25` and `N=25` objects/group.

### Output JSON structure

```json
{
  "method": "switch-message",
  "bandwidth_cap_bps": 0,
  "frame_interval_ms": 40,
  "objects_per_group": 25,
  "jitter_buffer_ms": 500,
  "aetr": 0.009416,
  "switches": [
    {
      "track_from": "3",
      "track_to": "4",
      "switch_delay_ms": 683,
      "delivery_gap_ms": 502,
      "stall_ms": 1000,
      "group_boundary_aligned": true,
      "control_messages": 1,
      "redundant_bytes": 0,
      "trailing_a_bytes": 46706,
      "useful_b_bytes": 4928553,
      "excess_bytes": 46706,
      "total_bytes": 4975259,
      "aetr": 0.009416,
      "a_objects_post_decision": 4,
      "last_a_group": 26,
      "first_b_group": 27
    }
  ]
}
```

### Metrics reference

| Field                     | Description                                                                                                                         |
| ------------------------- | ----------------------------------------------------------------------------------------------------------------------------------- |
| `switch_delay_ms`         | `t_first_b − t_decision` (ms). Negative = B pre-buffered, arrived before decision.                                                  |
| `delivery_gap_ms`         | `t_first_b − (t_last_a + 40ms)`. Signed playout gap. Negative = B arrived before A's slot ended.                                    |
| `stall_ms`                | `0` if gap ≤ JB; `gap − JB` if gap > JB. Measured freeze time: how long the player waited for the I-frame beyond the jitter budget. |
| `group_boundary_aligned`  | First B object had `object_id == 0` (I-frame).                                                                                      |
| `control_messages`        | Switch-specific control messages sent.                                                                                              |
| `redundant_bytes`         | Joining-Fetch warm-up bytes + pre-boundary B objects.                                                                               |
| `trailing_a_bytes`        | A bytes received after switch decision.                                                                                             |
| `useful_b_bytes`          | B bytes from first live group boundary onward.                                                                                      |
| `excess_bytes`            | `redundant_bytes + trailing_a_bytes`.                                                                                               |
| `total_bytes`             | `useful_b_bytes + excess_bytes`.                                                                                                    |
| `aetr`                    | `excess_bytes / total_bytes` per switch. Top-level `aetr` is mean across all switches.                                              |
| `a_objects_post_decision` | A objects that arrived after the decision.                                                                                          |

### Switch method summary

| Method             | CLI value            | Control msgs | Gap behavior                          | AETR   |
| ------------------ | -------------------- | ------------ | ------------------------------------- | ------ |
| SWITCH message     | `switch-message`     | 1            | Positive; = time to B's next boundary | Low    |
| Sub Update Forward | `sub-update-forward` | 3            | ≤ 0; A/B overlap                      | Medium |
| Joining Fetch      | `joining-fetch`      | 3–4          | null (fetch fills gap)                | High   |

### `client switch-test` flags

```
--track-sequence <s>        Comma-separated track list, e.g. "3,4"
--track-a / --track-b       Shorthand for a single switch (ignored when --track-sequence is set)
--method                    switch-message | sub-update-forward | joining-fetch
--switch-after <secs>       Seconds per track before triggering the next switch   [15]
--jitter-buffer-ms <ms>     Jitter budget; gap > JB triggers a GoP-length stall   [0]
--bandwidth-cap-bps <bps>   Recorded in JSON only; does not apply tc               [0]
--output-json <path>        Write result JSON to this path
```

### `client publish-multi` flags

```
--tracks <spec>             "name:avg_bytes[:p_ratio[:delay_ms]],..."
                            delay_ms creates an intra-GoP phase offset between tracks
                            Default: "1:2500,2:5000,3:12500,4:20000"
--objects-per-group <n>     Objects per 1-second GoP                              [25]
--interval <ms>             Inter-object gap                                       [40]
--group-count <n>           Total groups to publish                               [1000]
--publisher-priority <n>    QUIC stream priority                                  [128]
```

### `relay` flags

```
--port <n>                  QUIC listen port                [4433]
--host <addr>               Bind address                    [localhost]
--cert-file <path>          TLS certificate PEM
--key-file <path>           TLS private key PEM
--write-kbps-limit <n>      Soft per-subscriber cap (kbps)  [0 = off]
--cache-size <n>            Cached objects per track        [1000]
```
