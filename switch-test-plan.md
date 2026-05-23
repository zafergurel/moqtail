# Switch Test Plan

Empirical comparison of three subscriber-initiated track switching methods in MOQ Transport, for the paper _"Zero Gap, Zero Waste: Subscriber-Initiated, Relay-Executed Track Switching in MOQ Transport"_.

---

## Methods under test

| ID  | Method                       | Control messages | Who executes the switch   |
| --- | ---------------------------- | ---------------- | ------------------------- |
| A   | **Joining Fetch**            | 4–5              | Subscriber drives timing  |
| B   | **Sub Update Forward (0/1)** | 2–3              | Subscriber drives timing  |
| C   | **SWITCH message**           | 1                | Relay executes atomically |

### Method A — Joining Fetch

```
1. SUBSCRIBE B  (low priority, forward=true, latest-object)
2. JOINING_FETCH B  (relative, offset = N groups)
3. Receive fetch objects from B  → these are "warm-up" / redundant bytes
4. On first live B object: record t_first_b_object
5. REQUEST_UPDATE A  (end_group = last received A group)
6. REQUEST_UPDATE B  (raise priority)
```

### Method B — Sub Update Forward

```
1. SUBSCRIBE B  (forward=0, latest-object)
2. On next group boundary of A (object_id == 0):
   REQUEST_UPDATE A  (forward=0)
   REQUEST_UPDATE B  (forward=1)
   t_switch_decision = now
3. First B object → t_first_b_object
```

### Method C — SWITCH message

```
1. SWITCH (subscription_request_id=A.request_id, new track=B, params)
   t_switch_decision = now
   control_messages = 1
2. Relay atomically:
   - subscribes to B at next group boundary relative to A's last sent group
   - stops forwarding A
3. First B object → t_first_b_object
```

---

## Metrics

### Primary

| Metric                       | Formula                                      | Notes                                                                        |
| ---------------------------- | -------------------------------------------- | ---------------------------------------------------------------------------- |
| **Switch delay**             | `t_first_B_object − t_switch_decision`       | Time from sending the switch command to receiving the first new-track object |
| **Stall duration**           | `max(0, t_first_B_object − t_last_A_object)` | Positive = playback freeze; negative = overlap (no stall)                    |
| **Group boundary alignment** | `first_B_object.object_id == 0`              | Whether the switch landed cleanly on a group boundary                        |

### Secondary

| Metric                    | Definition                                                                              |
| ------------------------- | --------------------------------------------------------------------------------------- |
| **Redundant bytes**       | Bytes received from B before it becomes the active track (Joining Fetch warm-up period) |
| **Trailing bytes**        | Bytes from A received after the switch decision                                         |
| **Control message count** | Number of control-plane messages required to complete the switch                        |

### Per network condition

All primary and secondary metrics are collected at each point in the bandwidth and latency grids.

---

## Test topology

```
┌──────────────────────────────┐          ┌───────────────────────────┐
│  Relay host (Linux)          │  QUIC    │  Subscriber + Publisher   │
│                              │ ──────►  │  (same machine, macOS)    │
│  target/release/relay :4433  │  4433    │                           │
│                              │          │  target/release/client    │
│  tc / iptables               │          │  (switch-test command)    │
│  (shapes outbound UDP/4433   │          │                           │
│   toward subscriber IP)      │          │  target/release/moqtail-pub│
│                              │          │  ← ffmpeg multi-track pipe │
└──────────────────────────────┘          └───────────────────────────┘
```

- Relay is always remote (SSH'd from the subscriber machine).
- Publisher + subscriber run locally on the same machine.
- Publisher connects outbound to the relay (`https://relay-ip:4433`).
- tc rules on the relay shape traffic _toward_ the subscriber.
- Configuration lives in `scripts/.env` (gitignored); see `scripts/.env.example`.

---

## Track layout

Published by `scripts/ffmpeg.sh` (multi-track CMAF → `moqtail-pub`):

| CMAF track ID | MOQ track name | Resolution           | Bitrate  |
| ------------- | -------------- | -------------------- | -------- |
| 1             | `"1"`          | 1920×1080 (upscaled) | 4 Mbps   |
| 2             | `"2"`          | 1280×720 (native)    | 2.5 Mbps |
| 3             | `"3"`          | 854×480              | 1 Mbps   |
| 4             | `"4"`          | 640×360              | 500 kbps |
| 5             | `"5"`          | audio                | 128 kbps |

Primary test pair for the paper: **track `"2"` → track `"3"`** (720p→480p, 2.5× bitrate drop).  
Secondary pairs: `"3"→"4"` (downswitch) and `"3"→"2"` (upswitch on recovery).

---

## Experiment matrix

### Bandwidth conditions (`tc htb` on relay, UDP port 4433)

| Label     | Rate              | Notes                                                 |
| --------- | ----------------- | ----------------------------------------------------- |
| Baseline  | unlimited (no tc) | Reference; all methods should perform well            |
| High      | 5 Mbps            | Plenty for track 2 (2.5 Mbps); no pressure            |
| Tight     | 3 Mbps            | Slight pressure on track 2                            |
| Squeeze   | 2 Mbps            | At the limit; switch likely triggered                 |
| Low       | 1.5 Mbps          | Below track 2, above track 3                          |
| Congested | 1 Mbps            | Below track 3; tests behavior under severe congestion |

### Repetitions

**3 reps** per (method × bandwidth) condition → **3 × 6 × 3 = 54 runs** total.

### tc setup on relay (run via `experiment.sh`)

```bash
# Set limit toward subscriber
sudo bash scripts/tc/set_bandwidth.sh <rate_bps> <subscriber_ip> 1

# Remove limit
sudo bash scripts/tc/set_bandwidth.sh 0 <subscriber_ip> 1 del
```

Requires `INTERFACE` in `scripts/tc/.env` and passwordless `sudo` for `tc`/`iptables` on the relay host.

---

## Client implementation

### New files

| File                                  | Purpose                                                        |
| ------------------------------------- | -------------------------------------------------------------- |
| `apps/client/src/switcher.rs`         | Three switch method implementations + `SwitchStats` collection |
| _(update)_ `apps/client/src/stats.rs` | Add `SwitchStats` struct                                       |
| _(update)_ `apps/client/src/cli.rs`   | Add `SwitchTest` command + args                                |
| _(update)_ `apps/client/src/main.rs`  | Wire `Command::SwitchTest` → `switcher::run`                   |

### `SwitchStats` struct

```rust
pub struct SwitchStats {
    pub method: String,
    pub track_from: String,
    pub track_to: String,
    pub bandwidth_cap_bps: u64,          // 0 = no limit
    pub switch_decision_time: Instant,
    pub last_a_object_time: Option<Instant>,
    pub first_b_object_time: Option<Instant>,
    pub a_objects_post_decision: u64,
    pub b_objects_pre_active: u64,       // overlap / warm-up period
    pub redundant_bytes: u64,
    pub trailing_a_bytes: u64,
    pub control_messages: u64,
    pub group_boundary_aligned: Option<bool>,
    pub last_a_group: Option<u64>,
    pub first_b_group: Option<u64>,
}

impl SwitchStats {
    pub fn switch_delay_ms(&self) -> Option<i128> { ... }
    pub fn stall_ms(&self) -> Option<i128> { ... }  // negative = overlap, no stall
    pub fn to_json(&self) -> String { ... }          // written to --output-json file
}
```

### New CLI args

```
--command switch-test

--track-a <name>              Track name to subscribe to first (default: "2")
--track-b <name>              Track name to switch to (default: "3")
--method  <name>              joining-fetch | sub-update-forward | switch-message
--switch-after <secs>         Seconds before triggering switch (default: 15)
--joining-groups-offset <n>   Groups to prefetch in joining-fetch (default: 2)
--output-json <path>          Write SwitchStats as JSON to this file
```

### Implementation order

1. `SwitchStats` struct + JSON serialisation in `stats.rs`
2. `switch-message` method in `switcher.rs` (1 control message; relay already handles it fully)
3. CLI wiring — `Command::SwitchTest` in `cli.rs` + `main.rs`
4. End-to-end smoke test against the relay on Linux
5. `sub-update-forward` method (2–3 messages; `handle_request_update` in relay is ready)
6. `joining-fetch` method (4–5 messages; integrates with existing `fetcher.rs`)

---

## JSON output format (one file per run)

```json
{
  "method": "switch-message",
  "track_from": "2",
  "track_to": "3",
  "bandwidth_cap_bps": 2000000,
  "switch_delay_ms": 47,
  "stall_ms": -12,
  "group_boundary_aligned": true,
  "control_messages": 1,
  "redundant_bytes": 0,
  "trailing_a_bytes": 3840,
  "last_a_group": 42,
  "first_b_group": 43
}
```

Negative `stall_ms` means overlap (both tracks delivering simultaneously) — expected for Joining Fetch during the warm-up window.

Results directory layout:

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

---

## Running the experiments

### One-time Linux setup

```bash
# 1. SSH key
ssh-copy-id zafer@<relay-ip>

# 2. Passwordless sudo for tc on relay (append via visudo)
#    zafer ALL=(root) NOPASSWD: /usr/sbin/tc, /usr/sbin/iptables

# 3. Network interface for tc on relay
echo 'INTERFACE=eth0' > /home/zafer/projects/moqtail/moqtail/scripts/tc/.env

# 4. Build release binaries on relay
ssh zafer@<relay-ip> 'cd /home/zafer/projects/moqtail/moqtail && cargo build --release --bin relay'
```

### Local setup (subscriber + publisher machine)

```bash
# Copy and fill in SSH config
cp scripts/.env.example scripts/.env
# Edit scripts/.env: set RELAY_SSH and RELAY_PROJECT

# Build local binaries
cargo build --release --bin client --bin moqtail-pub
```

### Running

```bash
# Full experiment (build + start + 54 runs + stop)
bash scripts/experiment.sh --build

# Smoke test: one method, no tc, 1 rep (relay already running)
bash scripts/experiment.sh --skip-start --method switch-message --bandwidth 0 --reps 1

# Specific switch pair and single bandwidth
bash scripts/experiment.sh --track-from 3 --track-to 4 --bandwidth 1500000

# Watch relay logs in a separate terminal
ssh zafer@<relay-ip> 'tail -f /tmp/moqtail-relay.log /tmp/moqtail-pub.log'
```

---

## Key relay files (for reference)

| File                                                              | Relevance                                                      |
| ----------------------------------------------------------------- | -------------------------------------------------------------- |
| `apps/relay/src/server/message_handlers/subscribe_handler.rs:670` | `handle_switch_message` — relay's SWITCH implementation        |
| `apps/relay/src/server/client/switch_context.rs`                  | `SwitchStatus` state machine (None / Current / Next)           |
| `apps/relay/src/server/subscription.rs:654`                       | `check_switch_context` — per-object forwarding decision        |
| `apps/relay/src/server/message_handlers/subscribe_handler.rs:477` | `handle_request_update` — forward toggle for Sub Update method |
| `apps/relay/src/server/message_handlers/fetch_handler.rs`         | Joining Fetch relay handling                                   |
