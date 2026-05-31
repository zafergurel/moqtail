// Copyright 2025 The MOQtail Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::time::Instant;

use tracing::info;

// ─── Playout model ────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct PlayoutConfig {
  pub frame_interval_ms: u64,
  pub objects_per_group: u64,
  pub jitter_buffer_ms: u64,
}

impl PlayoutConfig {}

// ─── B I-frame acceptance verdict ────────────────────────────────────────────

pub enum BAcceptance {
  /// B arrived before its own PT and its group is ahead of last_a_object_at.
  /// Inner value is `last_a_object_at` at the moment of acceptance.
  Fresh((u64, u64)),
  /// B arrived at or after its own PT (`recv_offset >= PT(b_group, 0)`).
  /// Caller should use `record.last_a_group` (last actually-received A group)
  /// rather than the PT-computed threshold for switch metrics.
  Overdue,
  /// B arrived at or after its PT but its group is still behind the last played
  /// A group — wait for the specified target group before accepting.
  OverduePending(u64),
  /// B is behind A and has not yet reached its own PT — discard.
  Stale,
}

// ─── Player simulator ─────────────────────────────────────────────────────────
//
// Models a real video player's timing to compute presentation timestamps,
// stall duration, and skipped GOPs on a track switch.
//
// Presentation timestamp formula:
//   PT(group, object) = t_base
//                     + (group - first_group) * gop_ms
//                     + object * frame_interval_ms
//                     + jitter_buffer_ms
//
// where t_base = wall-clock Instant the first A I-frame was received and
// first_group = its group ID.

pub struct PlayerSimulator {
  frame_interval_ms: u64,
  objects_per_group: u64,
  jitter_buffer_ms: u64,
  t_base: Option<Instant>,
  first_group_time_b: Option<f64>,
  first_group_a: Option<u64>,
  first_group_b: Option<u64>,
}

impl PlayerSimulator {
  pub fn new(cfg: &PlayoutConfig) -> Self {
    Self {
      frame_interval_ms: cfg.frame_interval_ms,
      objects_per_group: cfg.objects_per_group,
      jitter_buffer_ms: cfg.jitter_buffer_ms,
      t_base: None,
      first_group_time_b: None,
      first_group_a: None,
      first_group_b: None,
    }
  }

  /// Call when the first A I-frame (object == 0) is received to set the clock base.
  /// Set the clock base to the first I-frame. Returns true the first time (base was just set).
  pub fn objects_per_group(&self) -> u64 {
    self.objects_per_group
  }

  pub fn set_base_if_unset(&mut self, group: u64, received_at: Instant) -> bool {
    if self.t_base.is_none() {
      self.t_base = Some(received_at);
      self.first_group_a = Some(group);
      info!(
        "PlayerSimulator: clock base set group={} t_base={:?}",
        group, received_at
      );
      true
    } else {
      false
    }
  }

  fn gop_ms(&self) -> f64 {
    (self.objects_per_group * self.frame_interval_ms) as f64
  }

  /// PT offset in ms from t_base for frame (group, object).
  /// PT in ms relative to t_base (first I-frame receive time).
  /// PT(first_group, 0) = JB (the jitter buffer delay before playback starts).
  /// Returns None until the first I-frame has been received.
  pub fn pt_ms(&self, group: u64, object: u64, for_b: bool) -> Option<f64> {
    self.pt_offset_ms(group, object, for_b)
  }

  fn pt_offset_ms(&self, group: u64, object: u64, for_b: bool) -> Option<f64> {
    let fg: u64 = if for_b {
      self.first_group_b?
    } else {
      self.first_group_a?
    };
    let group_rel = group as f64 - fg as f64;
    let offset = if for_b {
      self.first_group_time_b.unwrap_or(0.0)
    } else {
      0.0
    };
    Some(
      group_rel * self.gop_ms()
        + object as f64 * self.frame_interval_ms as f64
        + self.jitter_buffer_ms as f64
        + offset,
    )
  }

  /// Last A group whose I-frame PT has elapsed at wall-clock `now`.
  /// Returns None if the clock base has not been set or playback has not started.
  pub fn last_a_object_at(&self, now: Instant) -> Option<(u64, u64)> {
    let t_base = self.t_base?;
    let fg = self.first_group_a?;
    if now < t_base {
      return None;
    }
    let elapsed_ms = now.duration_since(t_base).as_secs_f64() * 1000.0;
    // PT(G, 0) = (G - fg) * gop_ms + JB <= elapsed_ms
    // (G - fg) <= (elapsed_ms - JB) / gop_ms
    let max_rel = (elapsed_ms - self.jitter_buffer_ms as f64) / self.gop_ms();
    if max_rel < 0.0 {
      return None;
    }
    let group = fg + max_rel.floor() as u64;
    let object = (((elapsed_ms - self.jitter_buffer_ms as f64) % self.gop_ms())
      / self.frame_interval_ms as f64)
      .floor() as u64;
    Some((group, object))
  }

  /// Classify a B I-frame arriving at `b_recv`.
  ///
  /// Priority order:
  ///   1. **Overdue** — `recv_offset >= PT(b_group, 0)`: the frame arrived at or
  ///      after its own presentation time; must accept even if b_group <= threshold.
  ///   2. **Fresh**  — `b_group > last_a_object_at(b_recv).map(|(g, _)| g).unwrap_or(0)`: arrived before its PT
  ///      but ahead of the last committed A group.
  ///   3. **Stale**  — neither; discard.
  pub fn check_b_iframe(&self, b_group: u64, b_recv: Instant) -> BAcceptance {
    let t_base = match self.t_base {
      Some(t) => t,
      // Clock not yet set — accept immediately.
      None => return BAcceptance::Fresh((0, 0)),
    };
    let recv_offset_ms = if b_recv >= t_base {
      b_recv.duration_since(t_base).as_millis() as f64
    } else {
      0.0
    };
    // compute PT based on A's timeline to determine if B is overdue
    if let Some(b_pt) = self.pt_offset_ms(b_group, 0, false)
      && recv_offset_ms >= b_pt
    {
      let target_group = self
        .last_a_object_at(b_recv)
        .map(|(group, _)| group)
        .or(self.first_group_a)
        .unwrap_or(0)
        + 1;
      if b_group >= target_group {
        info!(
          "PlayerSimulator: B I-frame OVERDUE group={} recv_offset={:.1} >= PT={:.1} target={}",
          b_group, recv_offset_ms, b_pt, target_group
        );
        return BAcceptance::Overdue;
      } else {
        info!(
          "PlayerSimulator: B I-frame OVERDUE_PENDING group={} < target={} recv_offset={:.1}",
          b_group, target_group, recv_offset_ms
        );
        return BAcceptance::OverduePending(target_group);
      }
    }
    let threshold = self
      .last_a_object_at(b_recv)
      .or(self.first_group_a.map(|g| (g, 0)))
      .unwrap_or((0, 0));

    if b_group > threshold.0 {
      info!(
        "PlayerSimulator: B I-frame FRESH group={} > threshold={:?}",
        b_group, threshold
      );
      return BAcceptance::Fresh(threshold);
    }
    info!(
      "PlayerSimulator: B I-frame STALE group={} threshold={:?} recv_offset={:.1}ms",
      b_group, threshold, recv_offset_ms
    );
    BAcceptance::Stale
  }

  /// Reset the clock base to the accepted B I-frame (call AFTER `compute_switch_metrics`).
  pub fn reset_base(&mut self, b_group: u64, received_at: Instant) {
    self.first_group_b = Some(b_group);

    if self.t_base.is_some() {
      self.first_group_time_b =
        Some(received_at.duration_since(self.t_base.unwrap()).as_millis() as f64);
    }

    // we don't reset t_base because we want to keep the original clock base
    // for consistent PT calculations and switch metrics,
    // but we update first_group to the new track's group so that PTs are computed relative to the new track's timeline.
  }

  /// Compute PT-based switch quality metrics when a fresh B I-frame is accepted.
  ///
  /// `last_a_group` is the last A group the player had committed to at that instant.
  ///
  /// Returns `(a_stopped_at_ms, b_started_at_ms, stall_ms, skipped_gops,
  /// skipped_duration_ms)` as ms offsets from t_base.  Returns None if the
  /// clock base has not been set.
  pub fn compute_switch_metrics(
    &self,
    last_a_group: u64,
    last_a_object: u64,
    b_group: u64,
    b_recv: Instant,
  ) -> Option<(f64, f64, u64, u64, u64)> {
    let t_base = self.t_base?;

    // a_stopped_at = end of the last A frame's display window = PT(last_a_object) + frame_interval
    let a_stopped_at =
      self.pt_offset_ms(last_a_group, last_a_object, false)? + self.frame_interval_ms as f64;

    // b_started_at = max(PT(b_group, 0), recv_offset + JB)
    let b_pt = self.pt_offset_ms(b_group, 0, true)?;
    let recv_offset_ms = if b_recv >= t_base {
      b_recv.duration_since(t_base).as_millis() as f64
    } else {
      0.0
    };
    let b_started_at = b_pt.max(recv_offset_ms + self.jitter_buffer_ms as f64);

    let stall_ms = ((b_started_at - a_stopped_at).max(0.0)) as u64;
    let skipped_gops = b_group.saturating_sub(last_a_group + 1);
    let skipped_duration_ms = skipped_gops * self.objects_per_group * self.frame_interval_ms;

    Some((
      a_stopped_at,
      b_started_at,
      stall_ms,
      skipped_gops,
      skipped_duration_ms,
    ))
  }
}

// ─── Per-switch record ───────────────────────────────────────────────────────

pub struct SwitchRecord {
  pub track_from: String,
  pub track_to: String,
  // Internal timing
  pub switch_decision_time: Option<Instant>,
  pub last_a_object_time: Option<Instant>,
  pub first_b_object_time: Option<Instant>,
  pub actual_switch_time: Option<Instant>,
  // Byte-level bookkeeping
  pub redundant_bytes: u64,
  /// A bytes received between switch decision and B I-frame acceptance.
  /// These fill the jitter buffer during the switch window and are not excess.
  pub pre_switch_a_bytes: u64,
  /// A bytes received after B I-frame was accepted (truly excess traffic).
  pub trailing_a_bytes: u64,
  pub useful_b_bytes: u64,
  /// Payload bytes of one complete GOP of the switched (B) track.
  pub b_gop_payload_bytes: Option<u64>,
  /// Accumulates bytes of the first accepted B group until it completes.
  pub b_cur_gop_bytes: u64,
  // Object counts
  pub a_objects_post_decision: u64,
  pub b_objects_pre_active: u64,
  // Control-plane overhead
  pub control_messages: u64,
  // Quality flags
  pub group_boundary_aligned: Option<bool>,
  pub decision_a_group: Option<u64>,
  pub decision_a_object: Option<u64>,
  /// Last A group the player had committed to at the moment B I-frame was accepted (PT-based).
  pub latest_played_a_group: Option<u64>,
  pub latest_played_a_object: Option<u64>,
  pub last_a_group: Option<u64>,
  pub last_a_object: Option<u64>,
  pub first_b_group: Option<u64>,
  pub switched_b_group: Option<u64>,
  // PT-based switch quality metrics
  pub a_stopped_at_ms: Option<f64>,
  pub b_started_at_ms: Option<f64>,
  pub skipped_gops: u64,
  pub skipped_duration_ms: u64,
}

impl SwitchRecord {
  pub fn new(track_from: &str, track_to: &str) -> Self {
    Self {
      track_from: track_from.to_string(),
      track_to: track_to.to_string(),
      switch_decision_time: None,
      last_a_object_time: None,
      first_b_object_time: None,
      actual_switch_time: None,
      redundant_bytes: 0,
      pre_switch_a_bytes: 0,
      trailing_a_bytes: 0,
      useful_b_bytes: 0,
      b_gop_payload_bytes: None,
      b_cur_gop_bytes: 0,
      a_objects_post_decision: 0,
      b_objects_pre_active: 0,
      control_messages: 0,
      group_boundary_aligned: None,
      decision_a_group: None,
      decision_a_object: None,
      latest_played_a_group: None,
      latest_played_a_object: None,
      last_a_group: None,
      last_a_object: None,
      first_b_group: None,
      switched_b_group: None,
      a_stopped_at_ms: None,
      b_started_at_ms: None,
      skipped_gops: 0,
      skipped_duration_ms: 0,
    }
  }

  /// Milliseconds between switch decision and first decodable B I-frame.
  pub fn switch_delay_ms(&self) -> Option<i128> {
    let decision = self.switch_decision_time?;
    let actual = self.actual_switch_time?;
    if actual >= decision {
      Some(actual.duration_since(decision).as_millis() as i128)
    } else {
      Some(-(decision.duration_since(actual).as_millis() as i128))
    }
  }

  pub fn last_a_object_time_ms(&self) -> Option<i128> {
    let decision = self.switch_decision_time?;
    let last_a = self.last_a_object_time?;
    if last_a >= decision {
      Some(last_a.duration_since(decision).as_millis() as i128)
    } else {
      Some(-(decision.duration_since(last_a).as_millis() as i128))
    }
  }

  pub fn first_b_object_time_ms(&self) -> Option<i128> {
    let decision = self.switch_decision_time?;
    let first_b = self.first_b_object_time?;
    if first_b >= decision {
      Some(first_b.duration_since(decision).as_millis() as i128)
    } else {
      Some(-(decision.duration_since(first_b).as_millis() as i128))
    }
  }

  /// PT-based viewer stall: how long the player froze waiting for the B I-frame.
  pub fn stall_ms(&self) -> Option<u64> {
    let b_started = self.b_started_at_ms?;
    let a_stopped = self.a_stopped_at_ms?;
    Some(((b_started - a_stopped).max(0.0)) as u64)
  }

  /// Excess bytes: redundant B warm-up + post-acceptance trailing A.
  pub fn excess_bytes(&self) -> u64 {
    self.redundant_bytes + self.trailing_a_bytes
  }

  /// Denominator for AETR: useful B + all excess bytes.
  pub fn total_bytes(&self) -> u64 {
    self.useful_b_bytes + self.excess_bytes()
  }

  /// Excess Traffic Ratio: excess_bytes / b_gop_payload_bytes.
  pub fn aetr(&self) -> Option<f64> {
    let gop = self.b_gop_payload_bytes? as f64;
    if gop == 0.0 {
      return None;
    }
    Some(self.excess_bytes() as f64 / gop)
  }

  fn opt_i128(v: Option<i128>) -> String {
    match v {
      Some(n) => n.to_string(),
      None => "null".to_string(),
    }
  }

  fn opt_u64(v: Option<u64>) -> String {
    match v {
      Some(n) => n.to_string(),
      None => "null".to_string(),
    }
  }

  fn opt_bool(v: Option<bool>) -> String {
    match v {
      Some(b) => b.to_string(),
      None => "null".to_string(),
    }
  }

  fn opt_f64(v: Option<f64>) -> String {
    match v {
      Some(f) => format!("{:.3}", f),
      None => "null".to_string(),
    }
  }

  pub fn to_json(&self) -> String {
    format!(
      concat!(
        "{{\n",
        "    \"track_from\": \"{}\",\n",
        "    \"track_to\": \"{}\",\n",
        "    \"switch_delay_ms\": {},\n",
        "    \"stall_ms\": {},\n",
        "    \"skipped_gops\": {},\n",
        "    \"skipped_duration_ms\": {},\n",
        "    \"group_boundary_aligned\": {},\n",
        "    \"control_messages\": {},\n",
        "    \"redundant_bytes\": {},\n",
        "    \"pre_switch_a_bytes\": {},\n",
        "    \"trailing_a_bytes\": {},\n",
        "    \"useful_b_bytes\": {},\n",
        "    \"b_gop_payload_bytes\": {},\n",
        "    \"excess_bytes\": {},\n",
        "    \"total_bytes\": {},\n",
        "    \"aetr\": {},\n",
        "    \"a_objects_post_decision\": {},\n",
        "    \"decision_a_group\": {},\n",
        "    \"decision_a_object\": {},\n",
        "    \"latest_played_a_group\": {},\n",
        "    \"latest_played_a_object\": {},\n",
        "    \"last_a_group\": {},\n",
        "    \"last_a_object\": {},\n",
        "    \"first_b_group\": {},\n",
        "    \"switched_b_group\": {},\n",
        "    \"a_stopped_at_ms\": {},\n",
        "    \"b_started_at_ms\": {},\n",
        "    \"last_a_object_time_ms\": {},\n",
        "    \"first_b_object_time_ms\": {}\n",
        "  }}"
      ),
      self.track_from,
      self.track_to,
      Self::opt_i128(self.switch_delay_ms()),
      Self::opt_u64(self.stall_ms()),
      Self::opt_u64(self.switched_b_group.map(|_| self.skipped_gops)),
      Self::opt_u64(self.switched_b_group.map(|_| self.skipped_duration_ms)),
      Self::opt_bool(self.group_boundary_aligned),
      self.control_messages,
      self.redundant_bytes,
      self.pre_switch_a_bytes,
      self.trailing_a_bytes,
      self.useful_b_bytes,
      Self::opt_u64(self.b_gop_payload_bytes),
      self.excess_bytes(),
      self.total_bytes(),
      Self::opt_f64(self.aetr()),
      self.a_objects_post_decision,
      Self::opt_u64(self.decision_a_group),
      Self::opt_u64(self.decision_a_object),
      Self::opt_u64(self.latest_played_a_group),
      Self::opt_u64(self.latest_played_a_object),
      Self::opt_u64(self.last_a_group),
      Self::opt_u64(self.last_a_object),
      Self::opt_u64(self.first_b_group),
      Self::opt_u64(self.switched_b_group),
      Self::opt_f64(self.a_stopped_at_ms),
      Self::opt_f64(self.b_started_at_ms),
      Self::opt_i128(self.last_a_object_time_ms()),
      Self::opt_i128(self.first_b_object_time_ms()),
    )
  }
}

// ─── Aggregate stats (one per run / JSON file) ────────────────────────────────

pub struct SwitchStats {
  pub method: String,
  pub bandwidth_cap_bps: u64,
  pub playout: PlayoutConfig,
  pub switches: Vec<SwitchRecord>,
  pub aetr: Option<f64>,
}

impl SwitchStats {
  pub fn new(method: &str, bandwidth_cap_bps: u64, playout: PlayoutConfig) -> Self {
    Self {
      method: method.to_string(),
      bandwidth_cap_bps,
      playout,
      switches: Vec::new(),
      aetr: None,
    }
  }

  pub fn finalize(&mut self) {
    let values: Vec<f64> = self.switches.iter().filter_map(|r| r.aetr()).collect();
    if !values.is_empty() {
      self.aetr = Some(values.iter().sum::<f64>() / values.len() as f64);
    }
  }

  pub fn to_json(&self) -> String {
    let switches_json: Vec<String> = self.switches.iter().map(|r| r.to_json()).collect();
    let switches_str = if switches_json.is_empty() {
      "[]".to_string()
    } else {
      format!("[\n  {}\n]", switches_json.join(",\n  "))
    };

    let aetr_str = match self.aetr {
      Some(f) => format!("{:.6}", f),
      None => "null".to_string(),
    };

    format!(
      concat!(
        "{{\n",
        "  \"method\": \"{}\",\n",
        "  \"bandwidth_cap_bps\": {},\n",
        "  \"frame_interval_ms\": {},\n",
        "  \"objects_per_group\": {},\n",
        "  \"jitter_buffer_ms\": {},\n",
        "  \"aetr\": {},\n",
        "  \"switches\": {}\n",
        "}}"
      ),
      self.method,
      self.bandwidth_cap_bps,
      self.playout.frame_interval_ms,
      self.playout.objects_per_group,
      self.playout.jitter_buffer_ms,
      aetr_str,
      switches_str,
    )
  }
}

// ─── Reception stats (used by subscribe command) ─────────────────────────────

pub struct ReceptionStats {
  pub total_received: u64,
  pub parse_errors: u64,
  pub sequence_gaps: u64,
  last_group: u64,
  last_object: u64,
  start_time: Instant,
}

impl ReceptionStats {
  pub fn new() -> Self {
    Self {
      total_received: 0,
      parse_errors: 0,
      sequence_gaps: 0,
      last_group: 0,
      last_object: 0,
      start_time: Instant::now(),
    }
  }

  pub fn record_object(&mut self, group_id: u64, object_id: u64) -> bool {
    self.total_received += 1;

    if self.total_received == 1 {
      self.last_group = group_id;
      self.last_object = object_id;
      return true;
    }

    let expected_next = (group_id == self.last_group && object_id == self.last_object + 1)
      || (group_id == self.last_group + 1 && object_id == 0);

    let sequence_ok = if expected_next {
      true
    } else {
      self.sequence_gaps += 1;
      false
    };

    self.last_group = group_id;
    self.last_object = object_id;
    sequence_ok
  }

  pub fn record_parse_error(&mut self) {
    self.parse_errors += 1;
  }

  pub fn elapsed_ms(&self) -> u128 {
    self.start_time.elapsed().as_millis()
  }

  pub fn report(&self) {
    info!(
      "Reception stats: total={}, last=group:{}/object:{}, errors={}, gaps={}, elapsed={}ms",
      self.total_received,
      self.last_group,
      self.last_object,
      self.parse_errors,
      self.sequence_gaps,
      self.elapsed_ms()
    );

    if self.parse_errors == 0 && self.sequence_gaps == 0 {
      info!("All objects received with correct sequence");
    } else {
      info!(
        "Found {} parse errors and {} sequence gaps",
        self.parse_errors, self.sequence_gaps
      );
    }
  }
}
