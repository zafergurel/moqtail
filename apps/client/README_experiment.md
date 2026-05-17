# Track Switching Experiment

Empirical comparison of three subscriber-initiated track switching methods in MOQ Transport, for the paper _"Zero Gap, Zero Waste: Subscriber-Initiated, Relay-Executed Track Switching in MOQ Transport"_.

---

## Overview

The `switch-test` command subscribes to a source track (track A), waits a configurable number of seconds, then switches to a target track (track B) using one of three methods. It records timing and byte-count statistics and writes them as JSON.

### Methods

| ID  | Method             | CLI value            | Control messages |
| --- | ------------------ | -------------------- | ---------------- |
| A   | Joining Fetch      | `joining-fetch`      | 4                |
| B   | Sub Update Forward | `sub-update-forward` | 3                |
| C   | SWITCH message     | `switch-message`     | 1                |

**Method A — Joining Fetch**

Subscriber pre-subscribes to B at low priority and sends a `JOINING_FETCH` to warm up N groups of history. When the first live B object arrives the subscriber stops A via `REQUEST_UPDATE`.

```
SUBSCRIBE B  (priority=200, forward=true)
JOINING_FETCH B  (relative, offset = joining-groups-offset)
← warm-up objects from B (redundant_bytes counted here)
← first live B object  →  t_first_b_object
REQUEST_UPDATE A  (forward=false)
REQUEST_UPDATE B  (priority=128)
```

**Method B — Sub Update Forward**

Subscriber pre-subscribes to B with `forward=false`. At the switch timer fires it enables B immediately, continues draining A, and tears down A once the first live B group boundary arrives — ensuring the decoder always starts from an I-frame.

```
SUBSCRIBE B  (forward=false)
... [switch-after seconds] ...
REQUEST_UPDATE B  (forward=true)       →  t_switch_decision
← A objects continue (trailing_a_bytes counted here)
← pre-boundary B objects (redundant_bytes counted here)
← first B object with object_id==0  →  t_first_b_object
REQUEST_UPDATE A  (forward=false)
```

**Method C — SWITCH message**

A single `SWITCH` message tells the relay to atomically substitute B for A at the next group boundary.

```
... [switch-after seconds] ...
SWITCH (subscription_request_id=A, new_track=B)  →  t_switch_decision
← SubscribeOk for B
← first B object  →  t_first_b_object
```

---

## Metrics collected

| Metric                    | Meaning                                                                     |
| ------------------------- | --------------------------------------------------------------------------- |
| `switch_latency_ms`       | `t_first_b_object − t_switch_decision`                                      |
| `delivery_gap_ms`         | `t_first_b − (t_last_a + frame_interval_ms)`; negative = overlap            |
| `freeze_ms`               | Realtime mode: freeze duration; `null` in VoD mode                          |
| `stall_ms`                | VoD mode: stall duration absorbed beyond playout buffer; `null` in realtime |
| `group_boundary_aligned`  | Whether the first B object had `object_id == 0`                             |
| `control_messages`        | Number of switch-specific control messages sent                             |
| `redundant_bytes`         | Bytes from the joining-fetch warm-up window                                 |
| `trailing_a_bytes`        | Bytes from A received after the switch decision                             |
| `a_objects_post_decision` | A objects that arrived after the switch decision                            |

---

## Prerequisites

### One-time relay setup (Linux host)

```bash
# 1. SSH key access
ssh-copy-id zafer@<relay-ip>

# 2. Passwordless sudo for tc on the relay (append via visudo)
#    zafer ALL=(root) NOPASSWD: /usr/sbin/tc, /usr/sbin/iptables

# 3. Network interface for tc shaping
echo 'INTERFACE=eth0' > /home/zafer/projects/moqtail/moqtail/scripts/tc/.env

# 4. Build relay binary on the relay host
ssh zafer@<relay-ip> 'cd /home/zafer/projects/moqtail/moqtail && cargo build --release --bin relay'
```

### Local setup (subscriber + publisher machine)

```bash
# Copy and fill in the SSH/project config
cp scripts/.env.example scripts/.env
# Edit scripts/.env: set RELAY_SSH and RELAY_PROJECT

# Build local binaries
cargo build --release --bin client --bin moqtail-pub
```

`scripts/.env.example`:

```bash
RELAY_SSH="user@relay-host-ip"
RELAY_PROJECT="/home/user/projects/moqtail/moqtail"
# RELAY_PORT=4433
# PUB_SSH=""          # empty = publisher runs locally
# PUB_PROJECT=""
# NAMESPACE="moqtail-watch-party-live"
```

---

## Running a single switch test manually

Start the relay and publisher first (or use `experiment.sh --skip-start` if they are already running), then:

```bash
./target/release/client \
  --server https://<relay-ip>:4433 \
  --namespace moqtail-watch-party-live \
  --command switch-test \
  --no-cert-validation \
  --track-a 2 \
  --track-b 3 \
  --method switch-message \
  --switch-after 15 \
  --output-json /tmp/result.json
```

The file `/tmp/result.json` will contain:

```json
{
  "method": "switch-message",
  "bandwidth_cap_bps": 0,
  "mode": "realtime",
  "frame_interval_ms": 40,
  "objects_per_group": 25,
  "jitter_buffer_ms": 0,
  "aetr": 0.18,
  "switches": [
    {
      "track_from": "2",
      "track_to": "3",
      "switch_latency_ms": 882,
      "delivery_gap_ms": 1,
      "freeze_ms": 1000,
      "stall_ms": null,
      "group_boundary_aligned": true,
      "control_messages": 1,
      "redundant_bytes": 0,
      "trailing_a_bytes": 245542,
      "useful_b_bytes": 1151722,
      "excess_bytes": 245542,
      "total_bytes": 1397264,
      "aetr": 0.18,
      "a_objects_post_decision": 22,
      "last_a_group": 32,
      "first_b_group": 33
    }
  ]
}
```

A positive `delivery_gap_ms` with `freeze_ms=1000` (one full GoP) is expected for SWITCH message (waits for the next group boundary). A negative `delivery_gap_ms` means overlap.

---

## Running the full experiment matrix

The orchestration script `scripts/experiment.sh` handles everything: builds binaries, starts relay + publisher via SSH, iterates over all method × bandwidth × repetition combinations, and writes one JSON file per run.

```bash
# Full run: build, start, 54 runs (3 methods × 6 bandwidths × 3 reps), stop
bash scripts/experiment.sh --build

# Smoke test: one method, no bandwidth cap, 1 rep (relay already running)
bash scripts/experiment.sh --skip-start --method switch-message --bandwidth 0 --reps 1

# Single bandwidth, all methods, 3 reps
bash scripts/experiment.sh --bandwidth 2000000

# Different track pair
bash scripts/experiment.sh --track-from 3 --track-to 4
```

Results are saved to `results/YYYYMMDD_HHMMSS/`:

```
results/
  20260510_143022/
    switch-message_0bps_rep1.json
    switch-message_0bps_rep2.json
    switch-message_0bps_rep3.json
    switch-message_5000000bps_rep1.json
    ...
    sub-update-forward_1000000bps_rep3.json
    joining-fetch_1000000bps_rep3.json
```

### Bandwidth conditions

| Label     | Rate      | Notes                                                  |
| --------- | --------- | ------------------------------------------------------ |
| Baseline  | 0 (no tc) | Reference; all methods should perform identically      |
| High      | 5 Mbps    | Plenty for track 3 (2.5 Mbps)                          |
| Tight     | 3 Mbps    | Slight pressure on track 3                             |
| Squeeze   | 2 Mbps    | At the limit; switch likely triggered by congestion    |
| Low       | 1.5 Mbps  | Below track 3, above track 2                           |
| Congested | 1 Mbps    | Below track 2; tests behaviour under severe congestion |

Bandwidth shaping is applied on the relay host toward the subscriber IP using `tc htb` + `iptables MARK`. See `scripts/tc/README.md` for manual `tc` commands.

### Watching relay logs during a run

```bash
ssh zafer@<relay-ip> 'tail -f /tmp/moqtail-relay.log /tmp/moqtail-pub.log'
```

---

## switch-test CLI reference

```
--command switch-test

--track-a <name>              Track to subscribe to initially          [default: 2]
--track-b <name>              Track to switch to                       [default: 3]
--method  <method>            joining-fetch | sub-update-forward | switch-message
                                                                        [default: switch-message]
--switch-after <secs>         Seconds before triggering the switch     [default: 15]
--joining-groups-offset <n>   Groups to prefetch in joining-fetch      [default: 2]
--bandwidth-cap-bps <bps>     Recorded in JSON only (0 = no limit)    [default: 0]
--output-json <path>          Write SwitchStats JSON to this file
```

---

## Track layout (published by `scripts/ffmpeg.sh`)

Track numbering: **lower number = lower bitrate** (video-only, no audio track).

| MOQ track name | Resolution | Bitrate  |
| -------------- | ---------- | -------- |
| `"1"`          | 640×360    | 500 kbps |
| `"2"`          | 854×480    | 1 Mbps   |
| `"3"`          | 1280×720   | 2.5 Mbps |
| `"4"`          | 1920×1080  | 4 Mbps   |

Primary test pair: **`"2"` → `"3"`** (480p → 720p, 2.5× bitrate increase).
Secondary pairs: `"3"→"4"` (upswitch) and `"3"→"2"` (downswitch).

All tracks use 1-second GOPs (`-g 25`) synchronised across streams so group boundaries are aligned — a clean group-boundary switch on one track corresponds to a clean boundary on any other.
