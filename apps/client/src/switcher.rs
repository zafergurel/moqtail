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

use crate::connection::MoqConnection;
use crate::stats::{BAcceptance, PlayerSimulator, SwitchRecord, SwitchStats};
use anyhow::Result;

use moqtail::model::common::location::Location;
use moqtail::model::common::tuple::{Tuple, TupleField};
use moqtail::model::control::constant::FetchType;
use moqtail::model::control::control_message::ControlMessage;
use moqtail::model::control::fetch::Fetch;
use moqtail::model::control::request_update::RequestUpdate;
use moqtail::model::control::subscribe::Subscribe;
use moqtail::model::control::switch::Switch;
use moqtail::model::control::unsubscribe::Unsubscribe;
use moqtail::model::parameter::message_parameter::MessageParameter;
use moqtail::transport::data_stream_handler::{FetchRequest, RecvDataStream};
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::{RwLock, mpsc};
use tokio::time::Duration;
use tracing::{info, warn};

// ─── Config ──────────────────────────────────────────────────────────────────

pub struct SwitchTestConfig {
  pub namespace: String,
  /// Comma-separated list of track names for a multi-switch run.
  /// Minimum 2 entries; supersedes track_a / track_b when non-empty.
  pub track_sequence: Vec<String>,
  /// Convenience aliases for a single two-track switch.
  pub track_a: String,
  pub track_b: String,
  pub method: SwitchMethod,
  pub switch_after_ms: u64,
  pub bandwidth_cap_bps: u64,
  pub output_json: Option<String>,
  pub playout: crate::stats::PlayoutConfig,
}

impl SwitchTestConfig {
  /// Resolve the ordered list of tracks for the experiment.
  pub fn sequence(&self) -> Vec<String> {
    if !self.track_sequence.is_empty() {
      self.track_sequence.clone()
    } else {
      vec![self.track_a.clone(), self.track_b.clone()]
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SwitchMethod {
  Switch,
  SubUpdateForward,
  JoiningFetch,
}

impl SwitchMethod {
  pub fn as_str(&self) -> &'static str {
    match self {
      SwitchMethod::Switch => "switch",
      SwitchMethod::SubUpdateForward => "sub-update-forward",
      SwitchMethod::JoiningFetch => "joining-fetch",
    }
  }
}

// ─── Internal types ───────────────────────────────────────────────────────────

struct ObjectEvent {
  track_alias: u64,
  group: u64,
  object: u64,
  payload_size: usize,
  received_at: Instant,
}

// ─── Event log ───────────────────────────────────────────────────────────────

struct EventLogInner {
  entries: Vec<(Instant, String)>,
  alias_labels: HashMap<u64, String>,
  t0: Option<Instant>,
}

#[derive(Clone)]
struct EventLog {
  inner: Arc<Mutex<EventLogInner>>,
}

impl EventLog {
  fn new() -> Self {
    Self {
      inner: Arc::new(Mutex::new(EventLogInner {
        entries: Vec::new(),
        alias_labels: HashMap::new(),
        t0: None,
      })),
    }
  }

  /// Set the log's time origin to the first I-frame arrival. All timestamps
  /// in the CSV will be relative to this instant (negative = arrived before
  /// the first I-frame, and therefore before playback could start).
  fn set_t0(&self, t: Instant) {
    let mut g = self.inner.lock().unwrap();
    if g.t0.is_none() {
      g.t0 = Some(t);
    }
  }

  fn register_alias(&self, alias: u64, label: &str) {
    self
      .inner
      .lock()
      .unwrap()
      .alias_labels
      .insert(alias, label.to_string());
  }

  fn record_object(&self, alias: u64, group: u64, object: u64, ts: Instant) {
    let mut g = self.inner.lock().unwrap();
    let label = g
      .alias_labels
      .get(&alias)
      .cloned()
      .unwrap_or_else(|| alias.to_string());
    g.entries
      .push((ts, format!("object,{},{},{}", label, group, object)));
  }

  fn record(&self, ts: Instant, line: impl Into<String>) {
    self.inner.lock().unwrap().entries.push((ts, line.into()));
  }

  fn flush(&self, path: &str) -> std::io::Result<()> {
    let g = self.inner.lock().unwrap();
    let mut entries = g.entries.clone();
    let t0 = g.t0.unwrap_or_else(|| {
      entries
        .first()
        .map(|(ts, _)| *ts)
        .unwrap_or_else(Instant::now)
    });
    drop(g);
    entries.sort_by_key(|(ts, _)| *ts);

    // Compute signed wall-clock ms for each entry.
    let with_ms: Vec<(i128, &str)> = entries
      .iter()
      .map(|(ts, line)| {
        let ms: i128 = if *ts >= t0 {
          ts.duration_since(t0).as_millis() as i128
        } else {
          -(t0.duration_since(*ts).as_millis() as i128)
        };
        (ms, line.as_str())
      })
      .collect();

    // Build PT lookup: key = "{label},{group},{object},{wall_ms}" -> pt_value string.
    // pt line format: pt,{label},{group},{object},{pt_ms}
    let mut pt_lookup: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for (ms, line) in &with_ms {
      if let Some(rest) = line.strip_prefix("pt,") {
        let mut it = rest.rsplitn(2, ',');
        if let (Some(pt_val), Some(prefix)) = (it.next(), it.next()) {
          pt_lookup.insert(format!("{},{}", prefix, ms), pt_val.to_string());
        }
      }
    }

    // Emit lines: skip pt rows; append PT to matching object rows.
    let mut content = String::new();
    for (ms, line) in &with_ms {
      if line.starts_with("pt,") {
        continue;
      }
      if let Some(rest) = line.strip_prefix("object,") {
        // key = "{label},{group},{object},{wall_ms}"
        let key = format!("{},{}", rest, ms);
        if let Some(pt) = pt_lookup.get(&key) {
          content.push_str(&format!("object,{},{},{}\n", rest, ms, pt));
        } else {
          content.push_str(&format!("object,{},{}\n", rest, ms));
        }
      } else {
        content.push_str(&format!("{},{}\n", line, ms));
      }
    }

    std::fs::write(path, content)
  }
}

/// Running state across switches.
struct SwitchState {
  /// Track alias of the currently active subscription.
  current_alias: u64,
  /// Request-ID of the currently active subscription.
  current_req_id: u64,
  /// Monotonically increasing counter for issuing new request-IDs.
  next_req_id: u64,
}

impl SwitchState {
  fn new(initial_alias: u64) -> Self {
    Self {
      current_alias: initial_alias,
      current_req_id: 0,
      next_req_id: 1,
    }
  }

  fn take_req_id(&mut self) -> u64 {
    let id = self.next_req_id;
    self.next_req_id += 1;
    id
  }
}

/// Per-switch outcome returned by each method's phase function.
struct PhaseResult {
  record: SwitchRecord,
  /// Alias of the newly active (B) track.
  new_alias: u64,
  /// Request-ID of the newly active subscription.
  new_req_id: u64,
}

// ─── Receiver task ────────────────────────────────────────────────────────────

async fn receiver_task(
  conn: Arc<wtransport::Connection>,
  pending_fetches: Arc<RwLock<BTreeMap<u64, FetchRequest>>>,
  tx: mpsc::UnboundedSender<ObjectEvent>,
  elog: EventLog,
) {
  loop {
    match conn.accept_uni().await {
      Ok(stream) => {
        let tx_clone = tx.clone();
        let pf_clone = pending_fetches.clone();
        let elog_clone = elog.clone();
        tokio::spawn(async move {
          let stream_handler = RecvDataStream::new(stream, pf_clone);
          let mut handler = &stream_handler;
          loop {
            let (next_handler, object) = handler.next_object().await;
            match object {
              Some(obj) => {
                let received_at = Instant::now();
                elog_clone.record_object(
                  obj.track_alias,
                  obj.location.group,
                  obj.location.object,
                  received_at,
                );
                let event = ObjectEvent {
                  track_alias: obj.track_alias,
                  group: obj.location.group,
                  object: obj.location.object,
                  payload_size: obj.payload.as_ref().map_or(0, |p| p.len()),
                  received_at,
                };
                if tx_clone.send(event).is_err() {
                  return;
                }
                handler = next_handler;
              }
              None => break,
            }
          }
        });
      }
      Err(e) => {
        info!("receiver_task: stream accept ended: {:?}", e);
        break;
      }
    }
  }
}

// ─── Control helpers ──────────────────────────────────────────────────────────

async fn subscribe_track(
  control_stream: &mut moqtail::transport::control_stream_handler::ControlStreamHandler,
  namespace: &str,
  track_name: &str,
  request_id: u64,
  priority: u8,
  forward: bool,
) -> Result<u64> {
  let ns = Tuple::from_utf8_path(namespace);
  let subscribe = Subscribe::new_latest_object(
    request_id,
    ns,
    TupleField::from_utf8(track_name),
    vec![
      MessageParameter::new_subscriber_priority(priority),
      MessageParameter::new_forward(forward),
    ],
  );
  control_stream
    .send(&ControlMessage::Subscribe(Box::new(subscribe)))
    .await
    .map_err(|e| anyhow::anyhow!("Subscribe send failed: {:?}", e))?;

  match control_stream.next_message().await {
    Ok(ControlMessage::SubscribeOk(m)) => {
      info!(
        "Subscribed to {}/{}: alias={}",
        namespace, track_name, m.track_alias
      );
      Ok(m.track_alias)
    }
    Ok(m) => anyhow::bail!("Expected SubscribeOk for {}, got {:?}", track_name, m),
    Err(e) => anyhow::bail!("Error waiting SubscribeOk for {}: {:?}", track_name, e),
  }
}

async fn send_request_update(
  cs: &mut moqtail::transport::control_stream_handler::ControlStreamHandler,
  update_req_id: u64,
  existing_req_id: u64,
  params: Vec<MessageParameter>,
) -> Result<()> {
  let ru = RequestUpdate::new(update_req_id, existing_req_id, params);
  cs.send(&ControlMessage::RequestUpdate(Box::new(ru)))
    .await
    .map_err(|e| anyhow::anyhow!("RequestUpdate send failed: {:?}", e))
}

// ─── Phase-3 event helpers ────────────────────────────────────────────────────

fn handle_a_object(
  ev: &ObjectEvent,
  record: &mut SwitchRecord,
  player: &mut PlayerSimulator,
  elog: &EventLog,
  first_b_seen: bool,
) {
  record.last_a_object_time = Some(ev.received_at);
  record.last_a_group = Some(ev.group);
  record.last_a_object = Some(ev.object);
  record.a_objects_post_decision += 1;
  if first_b_seen {
    record.trailing_a_bytes += ev.payload_size as u64;
  } else {
    record.pre_switch_a_bytes += ev.payload_size as u64;
  }
  if ev.object == 0 && player.set_base_if_unset(ev.group, ev.received_at) {
    elog.set_t0(ev.received_at);
  }
  if let Some(pt) = player.pt_ms(ev.group, ev.object, false) {
    elog.record(
      ev.received_at,
      format!("pt,A,{},{},{:.1}", ev.group, ev.object, pt),
    );
  }
}

/// Handle a single object from a B or Fetch source.
///
/// `pt_label`: event-log label — `"B"` for live B, `"F"` for fetch objects.
/// `initial_pt_for_b`: `false` → log A-timeline PT before reset_base (fetch);
///                     `true`  → returns None before reset_base, so no-op until acceptance (live B).
///
/// Returns `true` when a B I-frame is accepted so the caller can trigger
/// method-specific async side-effects (stop-A, elevate-B, set deadline).
fn process_b_object(
  ev: &ObjectEvent,
  player: &mut PlayerSimulator,
  record: &mut SwitchRecord,
  elog: &EventLog,
  overdue_target: &mut Option<u64>,
  first_b_seen: &mut bool,
  pt_label: &str,
  initial_pt_for_b: bool,
) -> bool {
  if let Some(pt) = player.pt_ms(ev.group, ev.object, initial_pt_for_b) {
    elog.record(
      ev.received_at,
      format!("pt,{},{},{},{:.1}", pt_label, ev.group, ev.object, pt),
    );
  }
  if record.first_b_object_time.is_none() {
    record.first_b_object_time = Some(ev.received_at);
    record.first_b_group = Some(ev.group);
  }
  if *first_b_seen {
    record.useful_b_bytes += ev.payload_size as u64;
    if record.b_gop_payload_bytes.is_none() {
      if ev.object == 0 {
        record.b_gop_payload_bytes = Some(record.b_cur_gop_bytes);
      } else {
        record.b_cur_gop_bytes += ev.payload_size as u64;
      }
    }
    return false;
  }
  if ev.object != 0 {
    record.redundant_bytes += ev.payload_size as u64;
    record.b_objects_pre_active += 1;
    return false;
  }
  let result = if let Some(tg) = *overdue_target {
    if ev.group >= tg {
      BAcceptance::Overdue
    } else {
      BAcceptance::Stale
    }
  } else {
    player.check_b_iframe(ev.group, ev.received_at)
  };
  match result {
    BAcceptance::Fresh(threshold) => {
      let (last_played_a_group, last_played_a_object) = threshold;
      record.latest_played_a_group = Some(last_played_a_group);
      record.latest_played_a_object = Some(last_played_a_object);
      player.reset_base(ev.group, ev.received_at);
      if let Some((a_ms, b_ms, stall, sg, sd)) = player.compute_switch_metrics(
        last_played_a_group,
        last_played_a_object,
        ev.group,
        ev.received_at,
      ) {
        record.a_stopped_at_ms = Some(a_ms);
        record.b_started_at_ms = Some(b_ms);
        record.skipped_gops = sg;
        record.skipped_duration_ms = sd;
        info!(
          "{}: PT metrics a_stopped={:.1} b_started={:.1} stall={}ms skipped_gops={}",
          pt_label, a_ms, b_ms, stall, sg
        );
      }
      if let Some(pt) = player.pt_ms(ev.group, 0, true) {
        elog.record(
          ev.received_at,
          format!("pt,{},{},0,{:.1}", pt_label, ev.group, pt),
        );
      }
      record.actual_switch_time = Some(ev.received_at);
      record.switched_b_group = Some(ev.group);
      record.group_boundary_aligned = Some(true);
      record.b_cur_gop_bytes = ev.payload_size as u64;
      *first_b_seen = true;
      elog.record(ev.received_at, format!("fresh_b_iframe,{}", ev.group));
      record.useful_b_bytes += ev.payload_size as u64;
      true
    }
    BAcceptance::Overdue => {
      let last_played_a_group = record
        .last_a_group
        .unwrap_or(record.decision_a_group.unwrap_or(0));
      let last_played_a_object = record
        .last_a_object
        .unwrap_or(record.decision_a_object.unwrap_or(0));
      record.latest_played_a_group = Some(last_played_a_group);
      record.latest_played_a_object = Some(last_played_a_object);
      player.reset_base(ev.group, ev.received_at);
      if let Some((a_ms, b_ms, stall, sg, sd)) = player.compute_switch_metrics(
        last_played_a_group,
        last_played_a_object,
        ev.group,
        ev.received_at,
      ) {
        record.a_stopped_at_ms = Some(a_ms);
        record.b_started_at_ms = Some(b_ms);
        record.skipped_gops = sg;
        record.skipped_duration_ms = sd;
        info!(
          "{}: PT metrics a_stopped={:.1} b_started={:.1} stall={}ms skipped_gops={}",
          pt_label, a_ms, b_ms, stall, sg
        );
      }
      if let Some(pt) = player.pt_ms(ev.group, 0, true) {
        elog.record(
          ev.received_at,
          format!("pt,{},{},0,{:.1}", pt_label, ev.group, pt),
        );
      }
      record.actual_switch_time = Some(ev.received_at);
      record.switched_b_group = Some(ev.group);
      record.group_boundary_aligned = Some(true);
      record.b_cur_gop_bytes = ev.payload_size as u64;
      *first_b_seen = true;
      elog.record(ev.received_at, format!("fresh_b_iframe,{}", ev.group));
      record.useful_b_bytes += ev.payload_size as u64;
      true
    }
    BAcceptance::OverduePending(tg) => {
      *overdue_target = Some(tg);
      record.b_objects_pre_active += 1;
      record.redundant_bytes += ev.payload_size as u64;
      false
    }
    BAcceptance::Stale => {
      record.b_objects_pre_active += 1;
      record.redundant_bytes += ev.payload_size as u64;
      false
    }
  }
}

// ─── Entry point ─────────────────────────────────────────────────────────────

pub async fn run(moq: MoqConnection, config: SwitchTestConfig) -> Result<()> {
  let MoqConnection {
    connection,
    mut control_stream,
  } = moq;

  let sequence = config.sequence();
  if sequence.len() < 2 {
    anyhow::bail!("track sequence must contain at least 2 tracks");
  }

  let pending_fetches: Arc<RwLock<BTreeMap<u64, FetchRequest>>> =
    Arc::new(RwLock::new(BTreeMap::new()));

  // Subscribe to the initial track (req_id=0, priority=128, forward=true).
  let initial_alias = subscribe_track(
    &mut control_stream,
    &config.namespace,
    &sequence[0],
    0,
    128,
    true,
  )
  .await?;

  // Start the background receiver task.
  let elog = EventLog::new();
  elog.register_alias(initial_alias, "A");
  let (tx, mut rx) = mpsc::unbounded_channel::<ObjectEvent>();
  let conn_clone = connection.clone();
  let pf_clone = pending_fetches.clone();
  let elog_rt = elog.clone();
  tokio::spawn(async move { receiver_task(conn_clone, pf_clone, tx, elog_rt).await });

  let mut state = SwitchState::new(initial_alias);
  let mut stats = SwitchStats::new(
    config.method.as_str(),
    config.bandwidth_cap_bps,
    config.playout.clone(),
  );

  // ── Multi-switch loop ──────────────────────────────────────────────────────
  for switch_idx in 0..(sequence.len() - 1) {
    let from_track = &sequence[switch_idx];
    let to_track = &sequence[switch_idx + 1];

    info!(
      "Switch {}: {} → {} (method={})",
      switch_idx + 1,
      from_track,
      to_track,
      config.method.as_str()
    );

    let phase = match config.method {
      SwitchMethod::Switch => {
        run_switch_phase(
          &mut control_stream,
          &mut rx,
          &mut state,
          from_track,
          to_track,
          &config,
          &elog,
        )
        .await?
      }

      SwitchMethod::SubUpdateForward => {
        // Pre-subscribe the target track with forward=false.
        let b_req_id = state.take_req_id();
        elog.record(Instant::now(), "subscribe_b");
        let b_alias = subscribe_track(
          &mut control_stream,
          &config.namespace,
          to_track,
          b_req_id,
          128,
          false,
        )
        .await?;
        elog.record(Instant::now(), "subscribe_ok_b");
        elog.register_alias(b_alias, "B");
        run_sub_update_forward_phase(
          &mut control_stream,
          &mut rx,
          &mut state,
          b_alias,
          b_req_id,
          from_track,
          to_track,
          &config,
          &elog,
        )
        .await?
      }

      SwitchMethod::JoiningFetch => {
        run_joining_fetch_phase(
          &mut control_stream,
          &mut rx,
          &mut state,
          &pending_fetches,
          from_track,
          to_track,
          &config,
          &elog,
        )
        .await?
      }
    };

    // Advance state for the next switch: the new current track becomes "A".
    state.current_alias = phase.new_alias;
    state.current_req_id = phase.new_req_id;
    elog.register_alias(phase.new_alias, "A");
    stats.switches.push(phase.record);
  }

  stats.finalize();

  let json = stats.to_json();
  info!("Switch test complete.\n{}", json);

  if let Some(ref path) = config.output_json {
    std::fs::write(path, &json)?;
    info!("Stats written to {}", path);
    let log_path = path.replace(".json", "_events.csv");
    if let Err(e) = elog.flush(&log_path) {
      warn!("Failed to write event log: {:?}", e);
    } else {
      info!("Event log written to {}", log_path);
    }
  }

  connection.close(0u32.into(), b"Done");
  Ok(())
}

// B I-frame acceptance and PT-based stall/skip metrics are handled by
// PlayerSimulator (stats.rs).  A B I-frame is accepted when its group ID
// exceeds the last A group the player has committed to at that instant.

// ─── Method A: SWITCH ────────────────────────────────────────────────────────

async fn run_switch_phase(
  cs: &mut moqtail::transport::control_stream_handler::ControlStreamHandler,
  rx: &mut mpsc::UnboundedReceiver<ObjectEvent>,
  state: &mut SwitchState,
  from_track: &str,
  to_track: &str,
  config: &SwitchTestConfig,
  elog: &EventLog,
) -> Result<PhaseResult> {
  let mut record = SwitchRecord::new(from_track, to_track);
  let current_alias = state.current_alias;
  let current_req_id = state.current_req_id;
  let mut player = PlayerSimulator::new(&config.playout);

  // Phase 1: receive current track objects for switch_after_ms.
  let deadline = tokio::time::Instant::now() + Duration::from_millis(config.switch_after_ms);
  loop {
    tokio::select! {
      _ = tokio::time::sleep_until(deadline) => {
        let now = Instant::now();
        record.switch_decision_time = Some(now);
        elog.record(now, "switch_decision");
        break
      },
      ev = rx.recv() => match ev {
        Some(ev) if ev.track_alias == current_alias => {
          record.last_a_object_time = Some(ev.received_at);
          record.last_a_group = Some(ev.group);
          record.last_a_object = Some(ev.object);
          if ev.object == 0
            && player.set_base_if_unset(ev.group, ev.received_at) { elog.set_t0(ev.received_at); }
          if let Some(pt) = player.pt_ms(ev.group, ev.object, false) {
            elog.record(ev.received_at, format!("pt,A,{},{},{:.1}", ev.group, ev.object, pt));
          }
        }
        Some(_) => {}
        None => break,
      }
    }
  }
  record.decision_a_group = record.last_a_group;
  record.decision_a_object = record.last_a_object;
  info!(
    "switch: decision fired, decision_a_group={:?}, decision_a_object={:?}",
    record.decision_a_group, record.decision_a_object
  );

  // Phase 2: send SWITCH message.
  let switch_req_id = state.take_req_id();
  let ns = Tuple::from_utf8_path(&config.namespace);
  let switch = Switch::new(
    switch_req_id,
    ns,
    TupleField::from_utf8(to_track),
    current_req_id,
    vec![],
  );
  cs.send(&ControlMessage::Switch(Box::new(switch)))
    .await
    .map_err(|e| anyhow::anyhow!("Switch send failed: {:?}", e))?;
  record.control_messages += 1;

  // Phase 3: wait for SubscribeOk for B, then collect post-switch objects.
  let post_deadline =
    tokio::time::Instant::now() + Duration::from_millis(config.switch_after_ms.max(10_000));
  let mut b_alias: Option<u64> = None;
  let mut first_b_seen = false;
  let mut overdue_target: Option<u64> = None;

  loop {
    tokio::select! {
      _ = tokio::time::sleep_until(post_deadline) => break,
      ev = rx.recv() => match ev {
        Some(ev) => {
          if ev.track_alias == current_alias {
            handle_a_object(&ev, &mut record, &mut player, elog, first_b_seen);
          } else if b_alias == Some(ev.track_alias) {
            process_b_object(&ev, &mut player, &mut record, elog,
                             &mut overdue_target, &mut first_b_seen, "B", true);
          }
        }
        None => break,
      },
      msg = cs.next_message() => match msg {
        Ok(ControlMessage::SubscribeOk(m)) => {
          info!("switch: SubscribeOk for {}, alias={}", to_track, m.track_alias);
          elog.record(Instant::now(), "subscribe_ok_b");
          elog.register_alias(m.track_alias, "B");
          b_alias = Some(m.track_alias);
        }
        Ok(other) => info!("switch: unexpected ctrl msg: {:?}", other),
        Err(e) => { warn!("switch: control stream error: {:?}", e); break; }
      }
    }
  }

  let new_alias = b_alias.unwrap_or(current_alias);
  Ok(PhaseResult {
    record,
    new_alias,
    new_req_id: switch_req_id,
  })
}

// ─── Method B: Sub Update Forward ────────────────────────────────────────────

async fn run_sub_update_forward_phase(
  cs: &mut moqtail::transport::control_stream_handler::ControlStreamHandler,
  rx: &mut mpsc::UnboundedReceiver<ObjectEvent>,
  state: &mut SwitchState,
  b_alias: u64,
  b_req_id: u64,
  from_track: &str,
  to_track: &str,
  config: &SwitchTestConfig,
  elog: &EventLog,
) -> Result<PhaseResult> {
  let mut record = SwitchRecord::new(from_track, to_track);
  record.control_messages += 1; // SUBSCRIBE B already sent
  let current_alias = state.current_alias;
  let current_req_id = state.current_req_id;
  let mut player = PlayerSimulator::new(&config.playout);

  // Phase 1: receive A objects for switch_after_ms.
  let deadline = tokio::time::Instant::now() + Duration::from_millis(config.switch_after_ms);
  loop {
    tokio::select! {
      _ = tokio::time::sleep_until(deadline) => {
        let now = Instant::now();
        record.switch_decision_time = Some(now);
        elog.record(now, "switch_decision");
        break
      },
      ev = rx.recv() => match ev {
        Some(ev) if ev.track_alias == current_alias => {
          record.last_a_object_time = Some(ev.received_at);
          record.last_a_group = Some(ev.group);
          record.last_a_object = Some(ev.object);
          if ev.object == 0
            && player.set_base_if_unset(ev.group, ev.received_at) { elog.set_t0(ev.received_at); }
          if let Some(pt) = player.pt_ms(ev.group, ev.object, false) {
            elog.record(ev.received_at, format!("pt,A,{},{},{:.1}", ev.group, ev.object, pt));
          }
        }
        Some(_) => {}
        None => break,
      }
    }
  }
  record.decision_a_group = record.last_a_group;
  record.decision_a_object = record.last_a_object;
  info!(
    "sub-update-forward: decision fired, decision_a_group={:?}, decision_a_object={:?}",
    record.decision_a_group, record.decision_a_object
  );

  // Phase 2: enable B (forward=true).
  let ru_req_b = state.take_req_id();
  elog.record(Instant::now(), "enable_b");
  send_request_update(
    cs,
    ru_req_b,
    b_req_id,
    vec![MessageParameter::new_forward(true)],
  )
  .await?;
  record.control_messages += 1;

  // Phase 3: drain A and B concurrently.
  // A keeps flowing and advancing the player model.
  // Tear down A only when the first FRESH B I-frame arrives.
  let post_deadline =
    tokio::time::Instant::now() + Duration::from_millis(config.switch_after_ms.max(10_000));
  let mut first_b_seen = false;
  let mut a_stopped = false;
  let mut post_b_deadline: Option<tokio::time::Instant> = None;
  let mut overdue_target: Option<u64> = None;

  loop {
    let maybe_post = post_b_deadline;
    tokio::select! {
      _ = tokio::time::sleep_until(post_deadline) => break,
      _ = async {
        if let Some(d) = maybe_post { tokio::time::sleep_until(d).await }
        else { std::future::pending::<()>().await }
      } => break,
      ev = rx.recv() => match ev {
        Some(ev) => {
          if ev.track_alias == current_alias {
            handle_a_object(&ev, &mut record, &mut player, elog, first_b_seen);
          } else if ev.track_alias == b_alias {
            if ev.object == 0 && !a_stopped {
              let ru_req_a = state.take_req_id();
              elog.record(Instant::now(), "stop_a");
              if let Err(e) = send_request_update(
                cs, ru_req_a, current_req_id,
                vec![MessageParameter::new_forward(false)],
              ).await {
                warn!("sub_update_forward: RequestUpdate A failed: {:?}", e);
                break;
              }
              record.control_messages += 1;
              a_stopped = true;
              record.a_stopped_at_ms = player.pt_ms(ev.group, 0, false);
              info!("sub-update-forward: sent RequestUpdate to stop A at {:?}", ev.received_at);
            }
            if process_b_object(&ev, &mut player, &mut record, elog,
                                &mut overdue_target, &mut first_b_seen, "B", true) {
              post_b_deadline = Some(tokio::time::Instant::now() + Duration::from_secs(5));
            }
          }
        }
        None => break,
      },
      msg = cs.next_message() => match msg {
        Ok(ControlMessage::RequestOk(m)) => info!("sub_update_forward: RequestOk: {:?}", m),
        Ok(other) => info!("sub_update_forward: unexpected ctrl: {:?}", other),
        Err(e) => { warn!("sub_update_forward: control error: {:?}", e); break; }
      }
    }
  }

  Ok(PhaseResult {
    record,
    new_alias: b_alias,
    new_req_id: b_req_id,
  })
}

// ─── Method C: Joining Fetch ──────────────────────────────────────────────────

async fn run_joining_fetch_phase(
  cs: &mut moqtail::transport::control_stream_handler::ControlStreamHandler,
  rx: &mut mpsc::UnboundedReceiver<ObjectEvent>,
  state: &mut SwitchState,
  pending_fetches: &Arc<RwLock<BTreeMap<u64, FetchRequest>>>,
  from_track: &str,
  to_track: &str,
  config: &SwitchTestConfig,
  elog: &EventLog,
) -> Result<PhaseResult> {
  let mut record = SwitchRecord::new(from_track, to_track);
  let current_alias = state.current_alias;
  let current_req_id = state.current_req_id;
  let mut player = PlayerSimulator::new(&config.playout);

  info!(
    "joining-fetch: Phase1 start, current_alias={}, switch_after_ms={}",
    current_alias, config.switch_after_ms
  );

  // Phase 1: collect A for switch_after_ms, then set switch_decision_time.
  let deadline = tokio::time::Instant::now() + Duration::from_millis(config.switch_after_ms);
  loop {
    tokio::select! {
      _ = tokio::time::sleep_until(deadline) => {
        let now = Instant::now();
        record.switch_decision_time = Some(now);
        record.decision_a_group = record.last_a_group;
        elog.record(now, "switch_decision");
        info!(
          "joining-fetch: decision fired, last_a_group={:?}, decision_a_group={:?}",
          record.last_a_group, record.decision_a_group
        );
        break;
      },
      ev = rx.recv() => match ev {
        Some(ev) if ev.track_alias == current_alias => {
          record.last_a_object_time = Some(ev.received_at);
          record.last_a_group = Some(ev.group);
          record.last_a_object = Some(ev.object);
          if ev.object == 0
            && player.set_base_if_unset(ev.group, ev.received_at) { elog.set_t0(ev.received_at); }
          if let Some(pt) = player.pt_ms(ev.group, ev.object, false) {
            elog.record(ev.received_at, format!("pt,A,{},{},{:.1}", ev.group, ev.object, pt));
          }
        }
        Some(ev) => {
          info!(
            "joining-fetch: Phase1 unexpected track_alias={} (want {}), group={}, object={}",
            ev.track_alias, current_alias, ev.group, ev.object
          );
        }
        None => return Ok(PhaseResult {
          record,
          new_alias: current_alias,
          new_req_id: current_req_id,
        }),
      }
    }
  }
  // Phase 2: send Subscribe-B (probe), await SubscribeOk, determine strategy.
  let b_req_id = state.take_req_id();
  let subscribe = Subscribe::new_latest_object(
    b_req_id,
    Tuple::from_utf8_path(&config.namespace),
    TupleField::from_utf8(to_track),
    vec![
      MessageParameter::new_subscriber_priority(200),
      MessageParameter::new_forward(true),
    ],
  );
  elog.record(Instant::now(), "subscribe_b");
  cs.send(&ControlMessage::Subscribe(Box::new(subscribe)))
    .await
    .map_err(|e| anyhow::anyhow!("Subscribe send failed: {:?}", e))?;
  record.control_messages += 1;

  let decision_a_group = record.decision_a_group.unwrap_or(0);

  let probe_ok = match cs.next_message().await {
    Ok(ControlMessage::SubscribeOk(m)) => m,
    Ok(m) => anyhow::bail!("Expected SubscribeOk for {}, got {:?}", to_track, m),
    Err(e) => anyhow::bail!("Error waiting SubscribeOk for {}: {:?}", to_track, e),
  };
  let probe_alias = probe_ok.track_alias;
  elog.record(Instant::now(), "subscribe_ok_b");
  elog.register_alias(probe_alias, "B");
  info!(
    "JoiningFetch: SubscribeOk for {}, alias={}",
    to_track, probe_alias
  );

  let largest = probe_ok.subscribe_parameters.iter().find_map(|p| {
    if let MessageParameter::LargestObject { location } = p {
      Some(location.clone())
    } else {
      None
    }
  });

  // Decide strategy based on B's current position relative to A.
  let (b_alias, active_b_req_id) = if let Some(loc) = largest {
    if loc.group < decision_a_group {
      // B is behind A: unsubscribe the probe and re-subscribe from the start of
      // A's current group. The relay will deliver once B reaches that group.
      info!(
        "JoiningFetch: B group {} < A group {}, re-subscribing from A's group start (no fetch)",
        loc.group, decision_a_group
      );
      cs.send(&ControlMessage::Unsubscribe(Box::new(Unsubscribe::new(
        b_req_id,
      ))))
      .await
      .map_err(|e| anyhow::anyhow!("Unsubscribe send failed: {:?}", e))?;
      record.control_messages += 1;

      let future_req_id = state.take_req_id();
      let subscribe_future = Subscribe::new_absolute_start(
        future_req_id,
        Tuple::from_utf8_path(&config.namespace),
        TupleField::from_utf8(to_track),
        Location {
          group: decision_a_group,
          object: 0,
        },
        vec![
          MessageParameter::new_subscriber_priority(128),
          MessageParameter::new_forward(true),
        ],
      );
      elog.record(Instant::now(), "subscribe_b_future");
      cs.send(&ControlMessage::Subscribe(Box::new(subscribe_future)))
        .await
        .map_err(|e| anyhow::anyhow!("Subscribe (future) send failed: {:?}", e))?;
      record.control_messages += 1;

      match cs.next_message().await {
        Ok(ControlMessage::SubscribeOk(m)) => {
          let future_alias = m.track_alias;
          elog.register_alias(future_alias, "B");
          info!("JoiningFetch: SubscribeOk (future) alias={}", future_alias);
          (future_alias, future_req_id)
        }
        Ok(m) => anyhow::bail!(
          "Expected SubscribeOk (future) for {}, got {:?}",
          to_track,
          m
        ),
        Err(e) => anyhow::bail!(
          "Error waiting SubscribeOk (future) for {}: {:?}",
          to_track,
          e
        ),
      }
    } else if loc.object > 0 {
      // B is mid-group and at or ahead of A: issue a Joining Fetch to deliver
      // [I-frame … LargestObject] so live B can start from the group boundary.
      let fetch_req_id = state.take_req_id();
      elog.register_alias(0, "F");
      let fetch = Fetch::new_joining(
        fetch_req_id,
        FetchType::AbsoluteFetch,
        b_req_id,
        record.last_a_group.unwrap_or(loc.group),
        vec![MessageParameter::new_subscriber_priority(50)],
      )
      .map_err(|e| anyhow::anyhow!("Fetch::new_joining: {}", e))?;
      elog.record(Instant::now(), "fetch_sent");
      cs.send(&ControlMessage::Fetch(Box::new(fetch.clone())))
        .await
        .map_err(|e| anyhow::anyhow!("Fetch send failed: {:?}", e))?;
      pending_fetches
        .write()
        .await
        .insert(fetch_req_id, FetchRequest::new(fetch_req_id, 0, fetch, 0));
      record.control_messages += 1;
      loop {
        match cs.next_message().await {
          Ok(ControlMessage::FetchOk(m)) => {
            info!("JoiningFetch FetchOk: {:?}", m);
            break;
          }
          Ok(ControlMessage::RequestOk(m)) => info!("JoiningFetch RequestOk: {:?}", m),
          Ok(ControlMessage::RequestError(e)) => {
            anyhow::bail!("JoiningFetch RequestError: {:?}", e)
          }
          Ok(m) => warn!("JoiningFetch: unexpected msg after Fetch: {:?}", m),
          Err(e) => anyhow::bail!("JoiningFetch: error reading FetchOk: {:?}", e),
        }
      }
      (probe_alias, b_req_id)
    } else {
      info!("JoiningFetch: B at group boundary, no fetch needed");
      (probe_alias, b_req_id)
    }
  } else {
    info!("JoiningFetch: no LargestObject in SubscribeOk");
    (probe_alias, b_req_id)
  };

  // Phase 3: collect B until first fresh I-frame, then 5-second tail.
  // Fetch objects (alias=0) and live B objects (alias=b_alias) are both subject
  // to the freshness check.  A objects continue arriving as trailing data and
  // advance the freshness threshold.
  let post_deadline =
    tokio::time::Instant::now() + Duration::from_millis(config.switch_after_ms.max(10_000));
  let mut first_b_seen = false;
  let mut a_stopped = false;
  let mut post_b_deadline: Option<tokio::time::Instant> = None;
  let mut overdue_target: Option<u64> = None;

  loop {
    let maybe_post = post_b_deadline;
    tokio::select! {
      _ = tokio::time::sleep_until(post_deadline) => break,
      _ = async {
        if let Some(d) = maybe_post { tokio::time::sleep_until(d).await }
        else { std::future::pending::<()>().await }
      } => break,
      ev = rx.recv() => match ev {
        Some(ev) => {
          if ev.track_alias == current_alias {
            handle_a_object(&ev, &mut record, &mut player, elog, first_b_seen);
          } else if ev.track_alias == 0 {
            // Fetch object.
            if ev.object == 0 && !a_stopped {
              let ru_a_req = state.take_req_id();
              elog.record(Instant::now(), "stop_a");
              if let Err(e) = send_request_update(
                cs, ru_a_req, current_req_id,
                vec![MessageParameter::new_forward(false)],
              ).await {
                warn!("joining_fetch: RequestUpdate A failed: {:?}", e);
                break;
              }
              record.control_messages += 1;
              a_stopped = true;
              info!("joining-fetch: sent RequestUpdate to stop A at {:?}", ev.received_at);
            }
            if process_b_object(&ev, &mut player, &mut record, elog,
                                &mut overdue_target, &mut first_b_seen, "F", false) {
              let ru_b_req = state.take_req_id();
              if let Err(e) = send_request_update(
                cs, ru_b_req, active_b_req_id,
                vec![MessageParameter::new_subscriber_priority(128)],
              ).await {
                warn!("joining_fetch: RequestUpdate B failed: {:?}", e);
                break;
              }
              record.control_messages += 1;
              post_b_deadline = Some(tokio::time::Instant::now() + Duration::from_secs(5));
            }
          } else if ev.track_alias == b_alias {
            // Live B object.
            if ev.object == 0 && !a_stopped {
              let ru_a_req = state.take_req_id();
              elog.record(Instant::now(), "stop_a");
              if let Err(e) = send_request_update(
                cs, ru_a_req, current_req_id,
                vec![MessageParameter::new_forward(false)],
              ).await {
                warn!("joining_fetch: RequestUpdate A failed: {:?}", e);
                break;
              }
              record.control_messages += 1;
              a_stopped = true;
              record.a_stopped_at_ms = player.pt_ms(ev.group, 0, false);
            }
            if process_b_object(&ev, &mut player, &mut record, elog,
                                &mut overdue_target, &mut first_b_seen, "B", true) {
              let ru_b_req = state.take_req_id();
              if let Err(e) = send_request_update(
                cs, ru_b_req, active_b_req_id,
                vec![MessageParameter::new_subscriber_priority(128)],
              ).await {
                warn!("joining_fetch: RequestUpdate B failed: {:?}", e);
                break;
              }
              record.control_messages += 1;
              post_b_deadline = Some(tokio::time::Instant::now() + Duration::from_secs(5));
            }
          }
        }
        None => break,
      },
      msg = cs.next_message() => match msg {
        Ok(ControlMessage::RequestOk(m)) => info!("joining_fetch: RequestOk: {:?}", m),
        Ok(other) => info!("joining_fetch: unexpected ctrl: {:?}", other),
        Err(e) => { warn!("joining_fetch: control error: {:?}", e); break; }
      }
    }
  }
  Ok(PhaseResult {
    record,
    new_alias: b_alias,
    new_req_id: active_b_req_id,
  })
}
