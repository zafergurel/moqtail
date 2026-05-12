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
use crate::stats::{SwitchRecord, SwitchStats};
use anyhow::Result;
use moqtail::model::common::tuple::{Tuple, TupleField};
use moqtail::model::control::constant::FetchType;
use moqtail::model::control::control_message::ControlMessage;
use moqtail::model::control::fetch::{Fetch, JoiningFetchProps};
use moqtail::model::control::request_update::RequestUpdate;
use moqtail::model::control::subscribe::Subscribe;
use moqtail::model::control::switch::Switch;
use moqtail::model::parameter::message_parameter::MessageParameter;
use moqtail::transport::data_stream_handler::{FetchRequest, RecvDataStream};
use std::collections::BTreeMap;
use std::sync::Arc;
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
  pub switch_after_secs: u64,
  pub joining_groups_offset: u64,
  pub bandwidth_cap_bps: u64,
  pub output_json: Option<String>,
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
  SwitchMessage,
  SubUpdateForward,
  JoiningFetch,
}

impl SwitchMethod {
  pub fn as_str(&self) -> &'static str {
    match self {
      SwitchMethod::SwitchMessage => "switch-message",
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
  tx: mpsc::Sender<ObjectEvent>,
) {
  loop {
    match conn.accept_uni().await {
      Ok(stream) => {
        let stream_handler = RecvDataStream::new(stream, pending_fetches.clone());
        let mut handler = &stream_handler;
        loop {
          let (next_handler, object) = handler.next_object().await;
          match object {
            Some(obj) => {
              let event = ObjectEvent {
                track_alias: obj.track_alias,
                group: obj.location.group,
                object: obj.location.object,
                payload_size: obj.payload.as_ref().map_or(0, |p| p.len()),
                received_at: Instant::now(),
              };
              if tx.send(event).await.is_err() {
                return;
              }
              handler = next_handler;
            }
            None => break,
          }
        }
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
  let (tx, mut rx) = mpsc::channel::<ObjectEvent>(2000);
  let conn_clone = connection.clone();
  let pf_clone = pending_fetches.clone();
  tokio::spawn(async move { receiver_task(conn_clone, pf_clone, tx).await });

  let mut state = SwitchState::new(initial_alias);
  let mut stats = SwitchStats::new(config.method.as_str(), config.bandwidth_cap_bps);

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
      SwitchMethod::SwitchMessage => {
        run_switch_message_phase(
          &mut control_stream,
          &mut rx,
          &mut state,
          from_track,
          to_track,
          &config,
        )
        .await?
      }

      SwitchMethod::SubUpdateForward => {
        // Pre-subscribe the target track with forward=false.
        let b_req_id = state.take_req_id();
        let b_alias = subscribe_track(
          &mut control_stream,
          &config.namespace,
          to_track,
          b_req_id,
          128,
          false,
        )
        .await?;
        run_sub_update_forward_phase(
          &mut control_stream,
          &mut rx,
          &mut state,
          b_alias,
          b_req_id,
          from_track,
          to_track,
          &config,
        )
        .await?
      }

      SwitchMethod::JoiningFetch => {
        // Pre-subscribe the target track at low priority, then send JOINING_FETCH.
        let b_req_id = state.take_req_id();
        let b_alias = subscribe_track(
          &mut control_stream,
          &config.namespace,
          to_track,
          b_req_id,
          200,
          true,
        )
        .await?;

        let fetch_req_id = state.take_req_id();
        let fetch = Fetch {
          request_id: fetch_req_id,
          fetch_type: FetchType::RelativeFetch,
          standalone_fetch_props: None,
          joining_fetch_props: Some(JoiningFetchProps {
            joining_request_id: b_req_id,
            joining_start: config.joining_groups_offset,
          }),
          parameters: vec![MessageParameter::new_subscriber_priority(200)],
        };
        control_stream
          .send(&ControlMessage::Fetch(Box::new(fetch.clone())))
          .await
          .map_err(|e| anyhow::anyhow!("Fetch send failed: {:?}", e))?;

        pending_fetches
          .write()
          .await
          .insert(fetch_req_id, FetchRequest::new(fetch_req_id, 0, fetch, 0));

        match control_stream.next_message().await {
          Ok(ControlMessage::FetchOk(m)) => info!("JoiningFetch FetchOk: {:?}", m),
          Ok(ControlMessage::RequestError(e)) => {
            anyhow::bail!("JoiningFetch RequestError: {:?}", e)
          }
          Ok(m) => warn!("JoiningFetch: unexpected msg after Fetch: {:?}", m),
          Err(e) => anyhow::bail!("JoiningFetch: error reading FetchOk: {:?}", e),
        }

        run_joining_fetch_phase(
          &mut control_stream,
          &mut rx,
          &mut state,
          b_alias,
          b_req_id,
          from_track,
          to_track,
          &config,
        )
        .await?
      }
    };

    // Advance state for the next switch.
    state.current_alias = phase.new_alias;
    state.current_req_id = phase.new_req_id;
    stats.switches.push(phase.record);
  }

  stats.finalize();

  let json = stats.to_json();
  info!("Switch test complete.\n{}", json);

  if let Some(ref path) = config.output_json {
    std::fs::write(path, &json)?;
    info!("Stats written to {}", path);
  }

  connection.close(0u32.into(), b"Done");
  Ok(())
}

// ─── Method A: SWITCH message ─────────────────────────────────────────────────

async fn run_switch_message_phase(
  cs: &mut moqtail::transport::control_stream_handler::ControlStreamHandler,
  rx: &mut mpsc::Receiver<ObjectEvent>,
  state: &mut SwitchState,
  from_track: &str,
  to_track: &str,
  config: &SwitchTestConfig,
) -> Result<PhaseResult> {
  let mut record = SwitchRecord::new(from_track, to_track);
  let current_alias = state.current_alias;
  let current_req_id = state.current_req_id;

  // Phase 1: receive current track objects for switch_after_secs.
  let deadline = tokio::time::Instant::now() + Duration::from_secs(config.switch_after_secs);
  loop {
    tokio::select! {
      _ = tokio::time::sleep_until(deadline) => break,
      ev = rx.recv() => match ev {
        Some(ev) if ev.track_alias == current_alias => {
          record.last_a_object_time = Some(ev.received_at);
          record.last_a_group = Some(ev.group);
        }
        Some(_) => {}
        None => break,
      }
    }
  }

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
  record.switch_decision_time = Some(Instant::now());
  record.control_messages += 1;

  // Phase 3: wait for SubscribeOk for B, then collect post-switch objects.
  let post_deadline =
    tokio::time::Instant::now() + Duration::from_secs(config.switch_after_secs.max(10));
  let mut b_alias: Option<u64> = None;

  loop {
    tokio::select! {
      _ = tokio::time::sleep_until(post_deadline) => break,
      ev = rx.recv() => match ev {
        Some(ev) => {
          if ev.track_alias == current_alias {
            record.last_a_object_time = Some(ev.received_at);
            record.trailing_a_bytes += ev.payload_size as u64;
            record.a_objects_post_decision += 1;
          } else if b_alias == Some(ev.track_alias) {
            if record.first_b_object_time.is_none() {
              record.first_b_object_time = Some(ev.received_at);
              record.first_b_group = Some(ev.group);
              record.group_boundary_aligned = Some(ev.object == 0);
              info!("First B object (switch-message): group={}, object={}", ev.group, ev.object);
            }
            record.useful_b_bytes += ev.payload_size as u64;
          }
        }
        None => break,
      },
      msg = cs.next_message() => match msg {
        Ok(ControlMessage::SubscribeOk(m)) => {
          info!("SWITCH: SubscribeOk for {}, alias={}", to_track, m.track_alias);
          b_alias = Some(m.track_alias);
          // Stop collecting post-switch data once we have the first B object
          // (post_deadline handles the overall timeout)
        }
        Ok(other) => info!("switch_message: unexpected ctrl msg: {:?}", other),
        Err(e) => { warn!("switch_message: control stream error: {:?}", e); break; }
      }
    }
  }

  let new_alias = b_alias.unwrap_or(current_alias);
  // After a SWITCH, the relay created subscription req_id = switch_req_id.
  Ok(PhaseResult {
    record,
    new_alias,
    new_req_id: switch_req_id,
  })
}

// ─── Method B: Sub Update Forward ────────────────────────────────────────────

async fn run_sub_update_forward_phase(
  cs: &mut moqtail::transport::control_stream_handler::ControlStreamHandler,
  rx: &mut mpsc::Receiver<ObjectEvent>,
  state: &mut SwitchState,
  b_alias: u64,
  b_req_id: u64,
  from_track: &str,
  to_track: &str,
  config: &SwitchTestConfig,
) -> Result<PhaseResult> {
  let mut record = SwitchRecord::new(from_track, to_track);
  record.control_messages += 1; // SUBSCRIBE B already sent
  let current_alias = state.current_alias;
  let current_req_id = state.current_req_id;

  // Phase 1: receive A objects for switch_after_secs; wait for group boundary after timer.
  let deadline = tokio::time::Instant::now() + Duration::from_secs(config.switch_after_secs);
  let boundary_deadline =
    tokio::time::Instant::now() + Duration::from_secs(config.switch_after_secs + 3);
  let mut timer_fired = false;

  loop {
    tokio::select! {
      _ = tokio::time::sleep_until(deadline), if !timer_fired => {
        timer_fired = true;
      }
      _ = tokio::time::sleep_until(boundary_deadline), if timer_fired => {
        // Grace period expired without a group boundary — switch anyway.
        break;
      }
      ev = rx.recv() => match ev {
        Some(ev) if ev.track_alias == current_alias => {
          record.last_a_object_time = Some(ev.received_at);
          record.last_a_group = Some(ev.group);
          if timer_fired && ev.object == 0 {
            break; // clean group boundary
          }
        }
        Some(_) => {}
        None => break,
      }
    }
  }

  // Phase 2: send REQUEST_UPDATE A (forward=false) then REQUEST_UPDATE B (forward=true).
  let ru_req_a = state.take_req_id();
  send_request_update(
    cs,
    ru_req_a,
    current_req_id,
    vec![MessageParameter::new_forward(false)],
  )
  .await?;
  record.control_messages += 1;

  let ru_req_b = state.take_req_id();
  send_request_update(
    cs,
    ru_req_b,
    b_req_id,
    vec![MessageParameter::new_forward(true)],
  )
  .await?;
  record.switch_decision_time = Some(Instant::now());
  record.control_messages += 1;

  // Phase 3: collect post-switch objects.
  let post_deadline =
    tokio::time::Instant::now() + Duration::from_secs(config.switch_after_secs.max(10));
  loop {
    tokio::select! {
      _ = tokio::time::sleep_until(post_deadline) => break,
      ev = rx.recv() => match ev {
        Some(ev) => {
          if ev.track_alias == current_alias {
            record.last_a_object_time = Some(ev.received_at);
            record.trailing_a_bytes += ev.payload_size as u64;
            record.a_objects_post_decision += 1;
          } else if ev.track_alias == b_alias {
            if record.first_b_object_time.is_none() {
              record.first_b_object_time = Some(ev.received_at);
              record.first_b_group = Some(ev.group);
              record.group_boundary_aligned = Some(ev.object == 0);
              info!("First B object (sub-update-forward): group={}, object={}", ev.group, ev.object);
            }
            record.useful_b_bytes += ev.payload_size as u64;
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
  rx: &mut mpsc::Receiver<ObjectEvent>,
  state: &mut SwitchState,
  b_alias: u64,
  b_req_id: u64,
  from_track: &str,
  to_track: &str,
  config: &SwitchTestConfig,
) -> Result<PhaseResult> {
  let mut record = SwitchRecord::new(from_track, to_track);
  // SUBSCRIBE B + JOINING_FETCH were already sent; record their cost.
  record.control_messages += 2;
  record.switch_decision_time = Some(Instant::now());
  let current_alias = state.current_alias;
  let current_req_id = state.current_req_id;

  let overall_deadline =
    tokio::time::Instant::now() + Duration::from_secs(config.switch_after_secs * 2 + 30);
  let mut first_b_seen = false;
  let mut post_b_deadline: Option<tokio::time::Instant> = None;

  loop {
    let maybe_post = post_b_deadline;
    tokio::select! {
      _ = tokio::time::sleep_until(overall_deadline) => break,
      _ = async {
        if let Some(d) = maybe_post { tokio::time::sleep_until(d).await }
        else { std::future::pending::<()>().await }
      } => break,
      ev = rx.recv() => match ev {
        Some(ev) => {
          if ev.track_alias == 0 {
            // Joining-fetch warm-up object.
            record.redundant_bytes += ev.payload_size as u64;
            record.b_objects_pre_active += 1;
          } else if ev.track_alias == current_alias {
            record.last_a_object_time = Some(ev.received_at);
            record.last_a_group = Some(ev.group);
            if first_b_seen {
              record.trailing_a_bytes += ev.payload_size as u64;
              record.a_objects_post_decision += 1;
            }
          } else if ev.track_alias == b_alias {
            if !first_b_seen {
              record.first_b_object_time = Some(ev.received_at);
              record.first_b_group = Some(ev.group);
              record.group_boundary_aligned = Some(ev.object == 0);
              first_b_seen = true;
              info!("First live B object (joining-fetch): group={}, object={}", ev.group, ev.object);

              // Stop A (forward=false), raise B priority.
              let ru_a_req = state.take_req_id();
              if let Err(e) = send_request_update(
                cs, ru_a_req, current_req_id,
                vec![MessageParameter::new_forward(false)],
              ).await {
                warn!("joining_fetch: RequestUpdate A failed: {:?}", e);
                break;
              }
              record.control_messages += 1;

              let ru_b_req = state.take_req_id();
              if let Err(e) = send_request_update(
                cs, ru_b_req, b_req_id,
                vec![MessageParameter::new_subscriber_priority(128)],
              ).await {
                warn!("joining_fetch: RequestUpdate B failed: {:?}", e);
                break;
              }
              record.control_messages += 1;

              // Collect trailing A for 5 more seconds then stop.
              post_b_deadline = Some(tokio::time::Instant::now() + Duration::from_secs(5));
            }
            record.useful_b_bytes += ev.payload_size as u64;
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
    new_req_id: b_req_id,
  })
}
