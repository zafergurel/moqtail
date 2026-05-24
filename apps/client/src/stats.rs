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

// ─── Per-switch record ───────────────────────────────────────────────────────

pub struct SwitchRecord {
  pub track_from: String,
  pub track_to: String,
  // Internal timing (used to compute derived fields, not serialised directly)
  pub switch_decision_time: Option<Instant>,
  pub last_a_object_time: Option<Instant>,
  /// Time the first B object of any kind arrived (any object_id).
  pub first_b_object_time: Option<Instant>,
  /// Time the first decodable B frame arrived (object_id == 0). Used for switch_delay_ms.
  pub actual_switch_time: Option<Instant>,
  // Byte-level bookkeeping
  /// Joining-fetch warm-up bytes (B objects delivered before first live B object)
  pub redundant_bytes: u64,
  /// A-track bytes that arrived after the switch decision
  pub trailing_a_bytes: u64,
  /// B-track bytes delivered from the first live B object onward
  pub useful_b_bytes: u64,
  // Object counts
  pub a_objects_post_decision: u64,
  pub b_objects_pre_active: u64,
  // Control-plane overhead
  pub control_messages: u64,
  // Quality flags
  pub group_boundary_aligned: Option<bool>,
  pub last_a_group: Option<u64>,
  pub first_b_group: Option<u64>,
  pub switched_b_group: Option<u64>,
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
      trailing_a_bytes: 0,
      useful_b_bytes: 0,
      a_objects_post_decision: 0,
      b_objects_pre_active: 0,
      control_messages: 0,
      group_boundary_aligned: None,
      last_a_group: None,
      first_b_group: None,
      switched_b_group: None,
    }
  }

  /// Milliseconds between switch decision and first decodable B frame (object_id == 0).
  /// Negative means the decodable frame arrived before the decision (JoiningFetch pre-warm-up case).
  pub fn switch_delay_ms(&self) -> Option<i128> {
    let decision = self.switch_decision_time?;
    let actual = self.actual_switch_time?;
    if actual >= decision {
      Some(actual.duration_since(decision).as_millis() as i128)
    } else {
      Some(-(decision.duration_since(actual).as_millis() as i128))
    }
  }

  /// Signed ms from switch decision to the last A object received.
  pub fn last_a_object_time_ms(&self) -> Option<i128> {
    let decision = self.switch_decision_time?;
    let last_a = self.last_a_object_time?;
    if last_a >= decision {
      Some(last_a.duration_since(decision).as_millis() as i128)
    } else {
      Some(-(decision.duration_since(last_a).as_millis() as i128))
    }
  }

  /// Signed ms from switch decision to the first B object received (any object_id).
  pub fn first_b_object_time_ms(&self) -> Option<i128> {
    let decision = self.switch_decision_time?;
    let first_b = self.first_b_object_time?;
    if first_b >= decision {
      Some(first_b.duration_since(decision).as_millis() as i128)
    } else {
      Some(-(decision.duration_since(first_b).as_millis() as i128))
    }
  }

  /// Gap between last A frame and first decodable B frame, minus one frame interval.
  /// Clamped to 0: negative means B arrived before the next expected slot (no gap).
  pub fn delivery_gap_ms(&self, frame_interval_ms: u64) -> Option<i128> {
    let last_a = self.last_a_object_time?;
    let actual = self.actual_switch_time?;
    let raw: i128 = if actual >= last_a {
      actual.duration_since(last_a).as_millis() as i128
    } else {
      -(last_a.duration_since(actual).as_millis() as i128)
    };
    Some((raw - frame_interval_ms as i128).max(0))
  }

  /// Stall duration in ms (jitter-buffer model).
  ///
  /// If the delivery gap is within the jitter budget the player absorbs it
  /// silently. If it exceeds the budget the player froze for exactly
  /// `gap − JB` ms waiting for the I-frame.
  pub fn stall_ms(&self, cfg: &PlayoutConfig) -> Option<u64> {
    let gap = self.delivery_gap_ms(cfg.frame_interval_ms)?;
    let past_budget = gap - cfg.jitter_buffer_ms as i128;
    if past_budget <= 0 {
      Some(0)
    } else {
      Some(past_budget as u64)
    }
  }

  /// Total excess bytes for this switch event (redundant warm-up + trailing A).
  pub fn excess_bytes(&self) -> u64 {
    self.redundant_bytes + self.trailing_a_bytes
  }

  /// Total bytes counted in the switch window = useful_b + excess.
  pub fn total_bytes(&self) -> u64 {
    self.useful_b_bytes + self.excess_bytes()
  }

  /// Average Excess Traffic Ratio for this single switch.
  /// None when no bytes were observed.
  pub fn aetr(&self) -> Option<f64> {
    let total = self.total_bytes();
    if total == 0 {
      None
    } else {
      Some(self.excess_bytes() as f64 / total as f64)
    }
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
      Some(f) => format!("{:.6}", f),
      None => "null".to_string(),
    }
  }

  pub fn to_json(&self, cfg: &PlayoutConfig) -> String {
    format!(
      concat!(
        "{{\n",
        "    \"track_from\": \"{}\",\n",
        "    \"track_to\": \"{}\",\n",
        "    \"switch_delay_ms\": {},\n",
        "    \"delivery_gap_ms\": {},\n",
        "    \"stall_ms\": {},\n",
        "    \"group_boundary_aligned\": {},\n",
        "    \"control_messages\": {},\n",
        "    \"redundant_bytes\": {},\n",
        "    \"trailing_a_bytes\": {},\n",
        "    \"useful_b_bytes\": {},\n",
        "    \"excess_bytes\": {},\n",
        "    \"total_bytes\": {},\n",
        "    \"aetr\": {},\n",
        "    \"a_objects_post_decision\": {},\n",
        "    \"last_a_group\": {},\n",
        "    \"first_b_group\": {},\n",
        "    \"switched_b_group\": {},\n",
        "    \"last_a_object_time_ms\": {},\n",
        "    \"first_b_object_time_ms\": {}\n",
        "  }}"
      ),
      self.track_from,
      self.track_to,
      Self::opt_i128(self.switch_delay_ms()),
      Self::opt_i128(self.delivery_gap_ms(cfg.frame_interval_ms)),
      Self::opt_u64(self.stall_ms(cfg)),
      Self::opt_bool(self.group_boundary_aligned),
      self.control_messages,
      self.redundant_bytes,
      self.trailing_a_bytes,
      self.useful_b_bytes,
      self.excess_bytes(),
      self.total_bytes(),
      Self::opt_f64(self.aetr()),
      self.a_objects_post_decision,
      Self::opt_u64(self.last_a_group),
      Self::opt_u64(self.first_b_group),
      Self::opt_u64(self.switched_b_group),
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
  /// Mean AETR across all switches; set by `finalize()`.
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

  /// Compute aggregate AETR from the recorded switch entries.
  pub fn finalize(&mut self) {
    let values: Vec<f64> = self.switches.iter().filter_map(|r| r.aetr()).collect();
    if !values.is_empty() {
      self.aetr = Some(values.iter().sum::<f64>() / values.len() as f64);
    }
  }

  pub fn to_json(&self) -> String {
    let switches_json: Vec<String> = self
      .switches
      .iter()
      .map(|r| r.to_json(&self.playout))
      .collect();
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

  /// Record a received object and validate sequence ordering.
  /// Returns true if the sequence is valid, false if a gap was detected.
  pub fn record_object(&mut self, group_id: u64, object_id: u64) -> bool {
    self.total_received += 1;

    if self.total_received == 1 {
      // First object, no sequence check needed
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
