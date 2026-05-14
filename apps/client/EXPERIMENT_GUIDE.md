# Track Switching Experiment Guide

How to build, run, and analyse the track-switching experiments from the terminal — from a single-machine smoke test to a full multi-computer bandwidth-conditioned matrix.

---

## Contents

1. [Build](#1-build)
2. [Local testing (single machine)](#2-local-testing-single-machine)
3. [Multi-machine testing](#3-multi-machine-testing)
4. [Running a single switch manually](#4-running-a-single-switch-manually)
5. [Analyzing results in the terminal](#5-analyzing-results-in-the-terminal)
6. [Reference](#6-reference)

---

## 1. Build

All commands are run from the workspace root (the directory that contains `Cargo.toml`).

```bash
# Debug build (fast compile, slower binary — good for development)
cargo build --bin relay --bin client

# Release build (required for experiments — use this for all timing measurements)
cargo build --release --bin relay --bin client
```

Binaries land in `target/release/` (or `target/debug/`).

The relay needs TLS certificates. Self-signed ones are included for development:

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

## 2. Local testing (single machine)

Local tests use `publish-multi` — a synthetic publisher that sends artificial bytes at realistic bitrates with realistic GOP structure (I-frame + P-frames). No real video, no SSH, no `tc`. This is the right starting point for development and baseline measurements.

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
  --tracks "2:12500,3:5000,4:2500"
```

**Terminal 3 — one switch test**

```bash
./target/release/client \
  --server https://127.0.0.1:4433 \
  --namespace moqtail-experiment \
  --command switch-test \
  --no-cert-validation \
  --method switch-message \
  --track-sequence "2,3" \
  --switch-after 5 \
  --output-json /tmp/result.json

cat /tmp/result.json
```

### 2.2 Using `local_test.sh`

`scripts/local_test.sh` handles everything: starts the relay and publisher, iterates over all methods and repetitions, saves one JSON per run.

```bash
# Full run: build release binaries, then 3 methods × 3 reps
bash scripts/local_test.sh --build

# Same but with a 3-track sequence (M=2 switches per run)
bash scripts/local_test.sh --build --track-sequence "2,3,4"

# Smoke test: one method, one rep, relay already running
bash scripts/local_test.sh --skip-start --method switch-message --reps 1

# Faster iteration: 5 seconds per track, 2 reps, only two methods
bash scripts/local_test.sh \
  --switch-after 5 --reps 2 \
  --method switch-message \
  --method joining-fetch
```

Results are written to `results/local_YYYYMMDD_HHMMSS/`, one file per run:

```
results/
  local_20260511_143022/
    switch-message_0bps_rep1.json
    switch-message_0bps_rep2.json
    switch-message_0bps_rep3.json
    sub-update-forward_0bps_rep1.json
    ...
    joining-fetch_0bps_rep3.json
```

### 2.3 Publisher frame sizes

`publish-multi` models the I-frame / P-frame size distribution of a real video GOP.  
Each track spec is `"name:avg_bytes"` or `"name:avg_bytes:p_ratio"` where `p_ratio` is  
the P-frame size as a fraction of the I-frame (default **0.25**, i.e. I-frame is 4× a P-frame):

```bash
# Default p_ratio=0.25 for all tracks
--tracks "2:12500,3:5000,4:2500"

# Flat objects (no GOP structure)
--tracks "2:12500:1.0,3:5000:1.0"

# Custom ratio
--tracks "2:12500:0.33,3:5000:0.33"    # I-frame ≈ 3× P-frame
```

With `N=25` objects/group and `p_ratio=0.25`, the sizes work out as:

| Track | avg B/obj | I-frame | P-frame |
| ----- | --------- | ------- | ------- |
| 2     | 12 500    | 44 643  | 11 161  |
| 3     | 5 000     | 17 857  | 4 464   |
| 4     | 2 500     | 8 929   | 2 232   |

---

## 3. Multi-machine testing

The multi-machine setup uses **real video** (`ffmpeg` + `moqtail-pub`) and optional `tc`
bandwidth shaping. The relay runs on a remote Linux host; the publisher runs locally or
on a second remote host.

### 3.1 One-time relay host setup

```bash
# 1. SSH key access (run from your subscriber machine)
ssh-copy-id zafer@<relay-ip>

# 2. Passwordless sudo for tc and iptables on the relay
#    Add via: sudo visudo
zafer ALL=(root) NOPASSWD: /usr/sbin/tc, /usr/sbin/iptables

# 3. Set the network interface for tc shaping
ssh zafer@<relay-ip> \
  "echo 'INTERFACE=eth0' > ~/projects/moqtail/moqtail/scripts/tc/.env"

# 4. Build relay binary on the relay host
ssh zafer@<relay-ip> \
  "cd ~/projects/moqtail/moqtail && cargo build --release --bin relay"

# 5. Start the relay
ssh zafer@<relay-ip> \
  "cd ~/projects/moqtail/moqtail && \
   nohup ./target/release/relay > /tmp/moqtail-relay.log 2>&1 &"
```

### 3.2 Local setup on the subscriber machine

```bash
# Copy and fill in the SSH/project config
cp scripts/.env.example scripts/.env
# Edit scripts/.env: set RELAY_SSH and RELAY_PROJECT at minimum

# Build the client
cargo build --release --bin client
```

Minimal `scripts/.env`:

```bash
RELAY_SSH="zafer@192.168.1.22"
RELAY_PROJECT="/home/zafer/projects/moqtail/moqtail"
# RELAY_PORT=4433
# PUB_SSH=""          # empty = publisher runs locally (same machine as subscriber)
# PUB_PROJECT=""
# NAMESPACE="moqtail-watch-party-live"
```

### 3.3 Starting the real-video publisher

The publisher reads a local MP4, encodes 5 parallel ABR tracks with `ffmpeg`, and pipes
the CMAF stream to `moqtail-pub`:

```bash
bash scripts/ffmpeg.sh \
  --url https://<relay-ip>:4433 \
  --input dev/source.mp4 \
  --release
```

Or let `experiment.sh` start it automatically (see below).

### 3.4 Running the full matrix with `experiment.sh`

```bash
# Full run: build everything, start relay + publisher, 54 runs (3 methods × 6 bandwidths × 3 reps)
bash scripts/experiment.sh --build

# Smoke test: relay already running, one method, no bandwidth cap, 1 rep
bash scripts/experiment.sh --skip-start --method switch-message --bandwidth 0 --reps 1

# Single bandwidth condition, all methods, 5 reps
bash scripts/experiment.sh --bandwidth 2000000 --reps 5

# Custom track pair
bash scripts/experiment.sh --track-from 3 --track-to 2   # upswitch (recovery)
```

Results go to `results/YYYYMMDD_HHMMSS/`.

### 3.5 Manual tc bandwidth shaping

Apply shaping on the relay host toward the subscriber's IP:

```bash
# On the relay host — limit outbound UDP to subscriber to 2 Mbps
ssh zafer@<relay-ip> \
  "cd ~/projects/moqtail/moqtail && \
   sudo bash scripts/tc/set_bandwidth.sh 2000000 <subscriber-ip> 1"

# Run tests …

# Remove the rule
ssh zafer@<relay-ip> \
  "cd ~/projects/moqtail/moqtail && \
   sudo bash scripts/tc/set_bandwidth.sh 0 <subscriber-ip> 1 del"
```

Watch tc stats live on the relay:

```bash
ssh zafer@<relay-ip> \
  "watch /sbin/tc -s -d class show dev eth0"
```

Tail relay and publisher logs:

```bash
ssh zafer@<relay-ip> "tail -f /tmp/moqtail-relay.log /tmp/moqtail-pub.log"
```

---

## 4. Running a single switch manually

Useful when you want to inspect one run in detail.

```bash
# Start relay and publisher first (or use --skip-start with experiment.sh).

./target/release/client \
  --server https://127.0.0.1:4433 \
  --namespace moqtail-experiment \
  --command switch-test \
  --no-cert-validation \
  --track-sequence "2,3,4" \
  --method joining-fetch \
  --switch-after 15 \
  --joining-groups-offset 2 \
  --bandwidth-cap-bps 0 \
  --output-json /tmp/result.json
```

More verbose output with `RUST_LOG`:

```bash
RUST_LOG=debug ./target/release/client \
  --server https://127.0.0.1:4433 \
  --namespace moqtail-experiment \
  --command switch-test \
  --no-cert-validation \
  --method switch-message \
  --track-sequence "2,3" \
  --switch-after 10
```

---

## 5. Analyzing results in the terminal

All result files are line-delimited JSON with the same shape.  
The examples below use `jq` (available via `brew install jq` / `apt install jq`).

### 5.1 Inspect a single file

```bash
cat results/local_20260511_143022/switch-message_0bps_rep1.json | jq .
```

```bash
# Just the per-switch records
jq '.switches[]' results/local_*/switch-message_0bps_rep1.json
```

### 5.2 Extract one metric from all files

```bash
# switch_latency_ms from every switch event in every file
jq -r '.switches[].switch_latency_ms' results/local_*/*.json

# One line per file: method TAB aetr
jq -r '[.method, (.aetr | tostring)] | join("\t")' results/local_*/*.json
```

### 5.3 Per-method summary (latency, stall, AETR)

```bash
for method in switch-message sub-update-forward joining-fetch; do
  echo "── $method ──────────────────────────"
  jq -r '.switches[] | [
      .switch_latency_ms,
      .stall_ms,
      (.aetr * 100),
      (.group_boundary_aligned | if . then 1 else 0 end)
    ] | map(tostring) | join("\t")' \
    results/local_*/${method}_*.json \
  | awk 'BEGIN{OFS="\t"}
         {lat+=$1; stall+=$2; aetr+=$3; gba+=$4; n++}
         END{printf "  n=%-3d  lat=%.0fms  stall=%.0fms  AETR=%.2f%%  gba=%.0f%%\n",
             n, lat/n, stall/n, aetr/n, gba/n*100}'
done
```

Sample output:

```
── switch-message ──────────────────────
  n=9    lat=521ms  stall=145ms  AETR=6.23%  gba=89%
── sub-update-forward ──────────────────
  n=9    lat=12ms   stall=-38ms  AETR=1.20%  gba=100%
── joining-fetch ───────────────────────
  n=9    lat=847ms  stall=-12ms  AETR=16.70%  gba=100%
```

### 5.4 Control message count per method

```bash
jq -r '[.method, (.switches[].control_messages | tostring)] | join("\t")' \
  results/local_*/*.json \
| sort | uniq -c | sort -k2
```

### 5.5 Latency by bandwidth condition (multi-machine results)

The filename encodes the bandwidth: `switch-message_2000000bps_rep1.json`.

```bash
# Extract bandwidth from filename, pivot latency by method and bandwidth
for f in results/20260511_*/switch-message_*bps*.json; do
  bw=$(basename "$f" | grep -oE '[0-9]+bps')
  lat=$(jq -r '.switches[].switch_latency_ms' "$f")
  echo -e "$bw\t$lat"
done | sort | awk '{sum[$1]+=$2; n[$1]++}
  END{for(bw in sum) printf "%-15s avg_lat=%dms\n", bw, sum[bw]/n[bw]}' | sort

# Same for all three methods
for method in switch-message sub-update-forward joining-fetch; do
  echo "=== $method ==="
  for f in results/20260511_*/${method}_*bps*.json; do
    bw=$(basename "$f" | grep -oE '[0-9]+bps')
    jq -r ".switches[] | \"$bw\t\(.switch_latency_ms)\t\(.stall_ms)\"" "$f"
  done | sort | awk 'BEGIN{OFS="\t"}
    {lat[$1]+=$2; stall[$1]+=$3; n[$1]++}
    END{for(bw in lat) printf "%-15s lat=%dms stall=%dms\n", bw, lat[bw]/n[bw], stall[bw]/n[bw]}' | sort
done
```

### 5.6 Group boundary alignment rate

```bash
# Fraction of switches that were group-boundary aligned, by method
for method in switch-message sub-update-forward joining-fetch; do
  echo -n "$method: "
  jq -r ".switches[].group_boundary_aligned" \
    results/local_*/${method}_*.json \
  | awk '{if($1=="true") yes++; else no++}
         END{printf "%d/%d = %.0f%%\n", yes, yes+no, yes/(yes+no)*100}'
done
```

### 5.7 Redundant and trailing bytes (Joining Fetch)

```bash
jq -r '[
  .switches[] |
  "from=\(.track_from) to=\(.track_to)  " +
  "redundant=\(.redundant_bytes)B  " +
  "trailing_a=\(.trailing_a_bytes)B  " +
  "AETR=\(.aetr * 100 | round)%"
][]' results/local_*/joining-fetch_*.json
```

### 5.8 Collecting all per-switch rows as TSV (for spreadsheet / R / Python)

```bash
echo -e "method\tbandwidth_cap_bps\trep\ttrack_from\ttrack_to\tswitch_latency_ms\tstall_ms\taetr\tgroup_boundary_aligned\tcontrol_messages\tredundant_bytes\ttrailing_a_bytes"

for f in results/**/*.json; do
  method=$(jq -r '.method' "$f")
  bw=$(jq -r '.bandwidth_cap_bps' "$f")
  rep=$(basename "$f" | grep -oE 'rep[0-9]+' | grep -oE '[0-9]+')
  jq -r --arg m "$method" --arg b "$bw" --arg r "$rep" '
    .switches[] |
    [$m, $b, $r,
     .track_from, .track_to,
     (.switch_latency_ms | tostring),
     (.stall_ms | tostring),
     (.aetr | tostring),
     (.group_boundary_aligned | tostring),
     (.control_messages | tostring),
     (.redundant_bytes | tostring),
     (.trailing_a_bytes | tostring)
    ] | join("\t")' "$f"
done
```

Redirect to a file and open in Numbers / Excel / `column -t`:

```bash
bash the_above_script.sh > results.tsv
column -t results.tsv | less -S
```

### 5.9 Quick Python stats (mean ± std per method)

```bash
python3 - <<'EOF'
import json, glob, statistics, collections

rows = collections.defaultdict(list)
for path in glob.glob("results/**/*.json", recursive=True):
    d = json.load(open(path))
    for sw in d["switches"]:
        rows[d["method"]].append({
            "latency": sw["switch_latency_ms"],
            "stall":   sw["stall_ms"],
            "aetr":    sw["aetr"] * 100,
            "aligned": sw["group_boundary_aligned"],
        })

for method, data in sorted(rows.items()):
    lats   = [r["latency"] for r in data if r["latency"] is not None]
    stalls = [r["stall"]   for r in data if r["stall"]   is not None]
    aetrs  = [r["aetr"]    for r in data]
    gba    = sum(1 for r in data if r["aligned"]) / len(data) * 100
    print(f"\n{method}  (n={len(data)})")
    print(f"  latency  : {statistics.mean(lats):.0f} ± {statistics.stdev(lats):.0f} ms"
          f"  [p50={statistics.median(lats):.0f}]")
    print(f"  stall    : {statistics.mean(stalls):.0f} ± {statistics.stdev(stalls):.0f} ms")
    print(f"  AETR     : {statistics.mean(aetrs):.2f} ± {statistics.stdev(aetrs):.2f} %")
    print(f"  boundary : {gba:.0f}%")
EOF
```

---

## 6. Reference

### Track layout

| MOQ track | Resolution | Avg bitrate | avg B/obj (25fps) | I-frame  | P-frame  |
| --------- | ---------- | ----------- | ----------------- | -------- | -------- |
| `"1"`     | 1920×1080  | 4 Mbps      | 20 000 B          | 71 429 B | 17 857 B |
| `"2"`     | 1280×720   | 2.5 Mbps    | 12 500 B          | 44 643 B | 11 161 B |
| `"3"`     | 854×480    | 1 Mbps      | 5 000 B           | 17 857 B | 4 464 B  |
| `"4"`     | 640×360    | 500 kbps    | 2 500 B           | 8 929 B  | 2 232 B  |
| `"5"`     | audio      | 128 kbps    | 640 B             | 640 B    | 640 B    |

I/P sizes computed with `p_ratio=0.25` and `N=25` objects/group.  
Primary test pair: **`"2"→"3"`** (downswitch).  
Secondary: `"3"→"4"` (downswitch), `"3"→"2"` (upswitch on recovery).

### Bandwidth conditions (experiment.sh)

| Label     | Rate      | Notes                        |
| --------- | --------- | ---------------------------- |
| Baseline  | 0 (no tc) | Reference                    |
| High      | 5 Mbps    | Comfortable for track 2      |
| Tight     | 3 Mbps    | Slight pressure              |
| Squeeze   | 2 Mbps    | At the limit                 |
| Low       | 1.5 Mbps  | Below track 2, above track 3 |
| Congested | 1 Mbps    | Below track 3                |

### Metrics

| Field                    | Description                                                                   |
| ------------------------ | ----------------------------------------------------------------------------- |
| `switch_latency_ms`      | `t_first_b_object − t_switch_decision` (negative = B arrived before decision) |
| `stall_ms`               | `t_first_b_object − t_last_a_object` (negative = overlap)                     |
| `group_boundary_aligned` | First B object had `object_id == 0`                                           |
| `control_messages`       | Switch-specific control messages sent                                         |
| `redundant_bytes`        | Joining-fetch warm-up bytes (B objects before first live B)                   |
| `trailing_a_bytes`       | A bytes received after the switch decision                                    |
| `useful_b_bytes`         | B bytes from first live B object onward                                       |
| `excess_bytes`           | `redundant_bytes + trailing_a_bytes`                                          |
| `total_bytes`            | `useful_b_bytes + excess_bytes`                                               |
| `aetr`                   | `excess_bytes / total_bytes` — per-switch Average Excess Traffic Ratio        |
| `aetr` (top-level)       | Mean AETR across all switches in the run                                      |

### Switch method summary

| Method             | CLI value            | Control msgs | Expected stall             | Expected AETR                             |
| ------------------ | -------------------- | ------------ | -------------------------- | ----------------------------------------- |
| SWITCH message     | `switch-message`     | 1            | > 0 (waits for next group) | Low                                       |
| Sub Update Forward | `sub-update-forward` | 3            | ≤ 0 (A/B overlap)          | Small (trailing A + pre-boundary B bytes) |
| Joining Fetch      | `joining-fetch`      | 4            | ≤ 0 (overlap possible)     | > 0 (warm-up bytes)                       |

### `client switch-test` flags

```
--track-sequence <s>        Comma-separated track list, e.g. "2,3,4"
--track-a / --track-b       Shorthand for a single switch (ignored when --track-sequence is set)
--method                    switch-message | sub-update-forward | joining-fetch
--switch-after <secs>       Seconds per track before triggering the next switch  [15]
--joining-groups-offset <n> Groups to prefetch in joining-fetch warm-up          [2]
--bandwidth-cap-bps <bps>   Recorded in JSON only; does not apply tc             [0]
--output-json <path>        Write result JSON to this path
```

### `client publish-multi` flags

```
--tracks <spec>             "name:avg_bytes[:p_ratio],..."  [1:20000,2:12500,3:5000,4:2500,5:640]
--objects-per-group <n>     Objects per 1-second GOP        [25]
--interval <ms>             Inter-object gap                [40]
--group-count <n>           Total groups to publish         [1000]
--publisher-priority <n>    QUIC stream priority            [128]
```

### `relay` flags

```
--port <n>                  QUIC listen port                [4433]
--host <addr>               Bind address                    [localhost]
--cert-file <path>          TLS certificate PEM             [apps/relay/cert/cert.pem]
--key-file <path>           TLS private key PEM             [apps/relay/cert/key.pem]
--write-kbps-limit <n>      Soft per-subscriber cap (kbps)  [0 = off]
--cache-size <n>            Cached objects per track        [1000]
```
