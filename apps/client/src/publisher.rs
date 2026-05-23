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

use crate::cli::ForwardingPreference;
use crate::connection::MoqConnection;
use crate::utils::should_log;
use anyhow::Result;
use bytes::Bytes;
use moqtail::model::common::location::Location;
use moqtail::model::common::tuple::{Tuple, TupleField};
use moqtail::model::control::constant::GroupOrder;
use moqtail::model::control::control_message::ControlMessage;
use moqtail::model::control::publish::Publish;
use moqtail::model::control::publish_namespace::PublishNamespace;
use moqtail::model::control::subscribe_ok::SubscribeOk;
use moqtail::model::data::datagram::Datagram;
use moqtail::model::data::object::Object;
use moqtail::model::data::subgroup_header::SubgroupHeader;
use moqtail::model::data::subgroup_object::SubgroupObject;
use moqtail::model::parameter::message_parameter::MessageParameter;
use moqtail::transport::control_stream_handler::ControlStreamHandler;
use moqtail::transport::data_stream_handler::{HeaderInfo, SendDataStream};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::{debug, error, info};

// ─── publish-multi types ──────────────────────────────────────────────────────

/// One entry in the multi-track spec: track name + per-object payload size + GOP shape.
#[derive(Clone, Debug)]
pub struct TrackSpec {
  pub name: String,
  /// Average payload size in bytes per object.  Controls the overall bitrate:
  ///   bitrate ≈ payload_size × objects_per_group × (1000 / interval_ms) × 8  bps
  pub payload_size: usize,
  /// P-frame size as a fraction of the I-frame size (0 < p_ratio ≤ 1.0).
  /// 0.25 means each P-frame is ¼ the size of the I-frame (I-frame is 4× larger).
  /// 1.0 means all objects are the same size (flat, no GOP structure).
  /// The average bitrate is preserved regardless of this value.
  pub p_ratio: f64,
  /// Milliseconds to delay before this track starts publishing its first object.
  /// All PUBLISH announcements are sent immediately; only data output is delayed.
  /// Used to create a controlled intra-GOP phase offset between tracks.
  pub start_delay_ms: u64,
}

impl TrackSpec {
  pub fn new(name: impl Into<String>, payload_size: usize) -> Self {
    Self {
      name: name.into(),
      payload_size,
      p_ratio: 0.25,
      start_delay_ms: 0,
    }
  }

  /// Compute (i_frame_bytes, p_frame_bytes) that honour both the average
  /// payload size and the P/I ratio for a GOP of `n` objects.
  ///
  /// Derivation:
  ///   avg = (I + (n-1)·r·I) / n  →  I = avg·n / (1 + (n-1)·r)
  ///                                   P = r·I
  pub fn gop_sizes(&self, n: u64) -> (usize, usize) {
    let r = self.p_ratio.clamp(f64::EPSILON, 1.0);
    if (r - 1.0).abs() < f64::EPSILON || n <= 1 {
      return (self.payload_size, self.payload_size);
    }
    let i = (self.payload_size as f64 * n as f64 / (1.0 + (n as f64 - 1.0) * r)).round() as usize;
    let p = (r * i as f64).round() as usize;
    (i.max(1), p.max(1))
  }
}

/// Config for the `publish-multi` command.
pub struct PublishMultiConfig {
  pub namespace: String,
  pub tracks: Vec<TrackSpec>,
  /// Objects emitted per group (group = 1-second GOP at 25fps → 25 objects).
  pub objects_per_group: u64,
  /// Inter-object interval in milliseconds (40 ms = 25 fps).
  pub interval_ms: u64,
  /// Total groups to publish (each group ≈ 1 second at default settings).
  pub group_count: u64,
  pub publisher_priority: u8,
}

pub struct PublishConfig {
  pub namespace: String,
  pub track_name: String,
  pub forwarding_preference: ForwardingPreference,
  pub group_count: u64,
  pub interval: u64,
  pub objects_per_group: u64,
  pub payload_size: usize,
  pub track_alias: u64,
  pub publisher_priority: u8,
  pub group_order: GroupOrder,
}

pub struct PublishNamespaceConfig {
  pub namespace: String,
  pub forwarding_preference: ForwardingPreference,
  pub group_count: u64,
  pub interval: u64,
  pub objects_per_group: u64,
  pub payload_size: usize,
  pub publisher_priority: u8,
}

pub async fn run_namespace(moq: MoqConnection, config: PublishNamespaceConfig) -> Result<()> {
  let MoqConnection {
    connection,
    mut control_stream,
  } = moq;

  let ns = Tuple::from_utf8_path(&config.namespace);

  // Step 1: Announce namespace
  publish_namespace(&mut control_stream, &ns).await?;

  let data_config = DataConfig {
    forwarding_preference: config.forwarding_preference,
    group_count: config.group_count,
    interval: config.interval,
    objects_per_group: config.objects_per_group,
    payload_size: config.payload_size,
    publisher_priority: config.publisher_priority,
  };

  // Step 2: Listen for Subscribe messages and serve data
  let mut track_alias_counter: u64 = 1;
  let mut tasks: Vec<tokio::task::JoinHandle<Result<()>>> = Vec::new();

  info!(
    "Waiting for Subscribe messages on namespace '{}'...",
    config.namespace
  );

  loop {
    match control_stream.next_message().await {
      Ok(ControlMessage::Subscribe(m)) => {
        let track_alias = track_alias_counter;
        track_alias_counter += 1;

        info!(
          "Received Subscribe: request_id={}, track={:?}, assigning track_alias={}",
          m.request_id, m.track_name, track_alias
        );

        let msg = SubscribeOk::new(m.request_id, track_alias, vec![], vec![]);

        control_stream.send_impl(&msg).await?;
        info!(
          "SubscribeOk sent for request_id={}, track_alias={}",
          m.request_id, track_alias
        );

        // Spawn a task to send data for this subscription
        let conn = connection.clone();
        let dc = data_config.clone();
        let task = tokio::spawn(async move { send_data(&conn, track_alias, &dc).await });
        tasks.push(task);
      }
      Ok(ControlMessage::Unsubscribe(m)) => {
        info!("Received Unsubscribe: {:?}", m);
      }
      Ok(m) => {
        info!("Received control message: {:?}", m);
      }
      Err(e) => {
        info!("Control stream ended: {:?}", e);
        break;
      }
    }
  }

  // Wait for all spawned data-sending tasks to complete
  for task in tasks {
    if let Err(e) = task.await {
      error!("Data sending task failed: {:?}", e);
    }
  }

  // Keep connection alive briefly to ensure delivery
  info!("Waiting before closing connection...");
  tokio::time::sleep(Duration::from_secs(2)).await;

  info!("Closing connection...");
  connection.close(0u32.into(), b"Done");

  Ok(())
}

pub async fn run(moq: MoqConnection, config: PublishConfig) -> Result<()> {
  let MoqConnection {
    connection,
    mut control_stream,
  } = moq;

  let ns = Tuple::from_utf8_path(&config.namespace);

  let data_config = DataConfig {
    forwarding_preference: config.forwarding_preference,
    group_count: config.group_count,
    interval: config.interval,
    objects_per_group: config.objects_per_group,
    payload_size: config.payload_size,
    publisher_priority: config.publisher_priority,
  };

  publish_track(
    &connection,
    &mut control_stream,
    &ns,
    &config.track_name,
    config.track_alias,
    config.group_order,
    &data_config,
  )
  .await?;

  // Keep connection alive briefly to ensure delivery
  info!("Waiting before closing connection...");
  tokio::time::sleep(Duration::from_secs(2)).await;

  info!("Closing connection...");
  connection.close(0u32.into(), b"Done");

  Ok(())
}

async fn publish_namespace(
  control_stream: &mut ControlStreamHandler,
  namespace: &Tuple,
) -> Result<()> {
  info!("Publishing namespace...");
  let publish_namespace = PublishNamespace::new(0, namespace.clone(), &[]);
  let expected_request_id = publish_namespace.request_id;

  control_stream
    .send(&ControlMessage::PublishNamespace(Box::new(
      publish_namespace,
    )))
    .await?;

  match control_stream.next_message().await {
    Ok(ControlMessage::RequestOk(ok)) if ok.request_id == expected_request_id => {
      info!("Namespace published successfully");
      Ok(())
    }
    Ok(ControlMessage::RequestOk(ok)) => {
      anyhow::bail!(
        "PublishNamespace got RequestOk for another request ID: expected {}, got {}",
        expected_request_id,
        ok.request_id
      )
    }
    Ok(m) => anyhow::bail!("Expected RequestOk, got {:?}", m),
    Err(e) => anyhow::bail!("Failed waiting for RequestOk: {:?}", e),
  }
}

#[derive(Clone)]
struct DataConfig {
  forwarding_preference: ForwardingPreference,
  group_count: u64,
  interval: u64,
  objects_per_group: u64,
  payload_size: usize,
  publisher_priority: u8,
}

async fn publish_track(
  connection: &Arc<wtransport::Connection>,
  control_stream: &mut ControlStreamHandler,
  namespace: &Tuple,
  track_name: &str,
  track_alias: u64,
  group_order: GroupOrder,
  data_config: &DataConfig,
) -> Result<()> {
  info!("Publishing track: track_alias={}", track_alias);
  let publish = Publish::new(
    0, // request_id
    namespace.clone(),
    TupleField::from_utf8(track_name),
    track_alias,
    vec![
      MessageParameter::new_group_order(group_order),
      MessageParameter::new_largest_object(Location::new(0, 0)),
      MessageParameter::Forward { forward: true },
    ],
    vec![],
  );
  control_stream
    .send(&ControlMessage::Publish(Box::new(publish)))
    .await?;

  match control_stream.next_message().await {
    Ok(ControlMessage::PublishOk(m)) => {
      info!("Track published, request_id: {}", m.request_id);
    }
    Ok(m) => anyhow::bail!("Expected PublishOk, got {:?}", m),
    Err(e) => anyhow::bail!("Failed waiting for PublishOk: {:?}", e),
  }

  send_data(connection, track_alias, data_config).await
}

async fn send_data(
  connection: &Arc<wtransport::Connection>,
  track_alias: u64,
  config: &DataConfig,
) -> Result<()> {
  match config.forwarding_preference {
    ForwardingPreference::Datagram => {
      send_datagrams(
        connection,
        track_alias,
        config.group_count,
        config.interval,
        config.objects_per_group,
        config.payload_size,
        config.publisher_priority,
      )
      .await
    }
    ForwardingPreference::Subgroup => {
      send_via_streams(
        connection,
        track_alias,
        config.group_count,
        config.interval,
        config.objects_per_group,
        config.payload_size,
        config.publisher_priority,
      )
      .await
    }
  }
}

async fn send_datagrams(
  connection: &wtransport::Connection,
  track_alias: u64,
  group_count: u64,
  interval_ms: u64,
  objects_per_group: u64,
  payload_size: usize,
  publisher_priority: u8,
) -> Result<()> {
  let interval = Duration::from_millis(interval_ms);
  info!(
    "Sending datagrams: {} groups, {} objects/group, {} byte payloads",
    group_count, objects_per_group, payload_size
  );

  for group_id in 0..group_count {
    for object_id in 0..objects_per_group {
      let payload = generate_payload(payload_size);

      let datagram_obj = Datagram::new_payload(
        track_alias,
        group_id,
        object_id,
        Some(publisher_priority), // publisher_priority
        None,                     // extension_headers
        Bytes::from(payload),
        false, // end_of_group
      );

      let serialized = datagram_obj.serialize()?;

      match connection.send_datagram(serialized) {
        Ok(_) => {
          let total = group_id * objects_per_group + object_id;
          if should_log(total) {
            info!(
              "Sent datagram: group={}, object={}, size={} bytes",
              group_id, object_id, payload_size
            );
          } else {
            debug!("Sent datagram: group={}, object={}", group_id, object_id);
          }
        }
        Err(e) => {
          error!(
            "Failed to send datagram: group={}, object={}, error={:?}",
            group_id, object_id, e
          );
        }
      }

      tokio::time::sleep(interval).await;
    }
  }

  info!("All datagrams sent");
  Ok(())
}

async fn send_via_streams(
  connection: &wtransport::Connection,
  track_alias: u64,
  group_count: u64,
  interval_ms: u64,
  objects_per_group: u64,
  payload_size: usize,
  publisher_priority: u8,
) -> Result<()> {
  let interval = Duration::from_millis(interval_ms);
  info!(
    "Sending via streams: {} groups, {} objects/group, {} byte payloads",
    group_count, objects_per_group, payload_size
  );

  for group_id in 0..group_count {
    info!("Opening stream for group {}", group_id);
    let stream = connection.open_uni().await?.await?;

    let sub_header = SubgroupHeader::new_with_explicit_id(
      track_alias,
      group_id,
      1u64,
      Some(publisher_priority),
      true,
      true,
    );
    let header_info = HeaderInfo::Subgroup { header: sub_header };
    let stream = Arc::new(Mutex::new(stream));
    let mut handler = SendDataStream::new(stream, header_info).await?;

    let mut prev_object_id = None;
    for object_id in 0..objects_per_group {
      let payload = generate_payload(payload_size);

      let subgroup_obj = SubgroupObject {
        object_id,
        extension_headers: Some(vec![]),
        object_status: None,
        payload: Some(Bytes::from(payload)),
      };
      let object =
        Object::try_from_subgroup(subgroup_obj, track_alias, group_id, Some(group_id), Some(1))?;

      match handler.send_object(&object, prev_object_id).await {
        Ok(_) => {
          let total = group_id * objects_per_group + object_id;
          if should_log(total) {
            info!(
              "Sent object: group={}, object={}, size={} bytes",
              group_id, object_id, payload_size
            );
          } else {
            debug!("Sent object: group={}, object={}", group_id, object_id);
          }
        }
        Err(e) => {
          error!(
            "Failed to send object: group={}, object={}, error={:?}",
            group_id, object_id, e
          );
        }
      }
      prev_object_id = Some(object_id);
      tokio::time::sleep(interval).await;
    }

    handler.flush().await?;
    info!("Stream flushed for group {}", group_id);
  }

  info!("All streams sent");
  Ok(())
}

fn generate_payload(size: usize) -> Vec<u8> {
  // Simple PRNG for reproducible test payloads
  let mut seed: u64 = 0x123456789abcdef0;
  (0..size)
    .map(|_| {
      seed ^= seed << 13;
      seed ^= seed >> 7;
      seed ^= seed << 17;
      (seed & 0xFF) as u8
    })
    .collect()
}

// ─── publish-multi ────────────────────────────────────────────────────────────

/// Publish multiple tracks simultaneously in push (PUBLISH) mode with
/// synchronized timing so that all tracks stay at the same group counter.
///
/// This is the correct publisher mode for the `switch-test --method switch-message`
/// experiment: the relay caches objects for every track from the start, so when a
/// SWITCH message fires, the relay can immediately find the target track's latest
/// object at the current group.
pub async fn run_multi(moq: MoqConnection, config: PublishMultiConfig) -> Result<()> {
  let MoqConnection {
    connection,
    mut control_stream,
  } = moq;

  let ns = Tuple::from_utf8_path(&config.namespace);

  if config.tracks.is_empty() {
    anyhow::bail!("publish-multi: --tracks must specify at least one track");
  }

  // Send PUBLISH + wait PublishOk for each track sequentially.
  // Aliases are assigned 1-based (1, 2, 3, …).
  let mut aliases: Vec<u64> = Vec::with_capacity(config.tracks.len());
  for (i, spec) in config.tracks.iter().enumerate() {
    let alias = (i as u64) + 1;
    send_publish_header(
      &mut control_stream,
      &ns,
      &spec.name,
      alias,
      GroupOrder::Ascending,
      config.publisher_priority,
    )
    .await?;
    aliases.push(alias);
    info!(
      "publish-multi: PUBLISH accepted for track='{}' alias={} payload={}B",
      spec.name, alias, spec.payload_size
    );
  }

  // All data tasks anchor to the same start instant so their group/object
  // counters stay in sync regardless of task scheduling jitter.
  let start = tokio::time::Instant::now();

  let mut tasks = Vec::with_capacity(config.tracks.len());
  for (spec, &alias) in config.tracks.iter().zip(aliases.iter()) {
    let conn = connection.clone();
    let spec = spec.clone();
    let priority = config.publisher_priority;
    let group_count = config.group_count;
    let objects_per_group = config.objects_per_group;
    let interval_ms = config.interval_ms;
    let track_name = spec.name.clone();

    let effective_start = start + Duration::from_millis(spec.start_delay_ms);
    tasks.push(tokio::spawn(async move {
      send_multi_track(
        &conn,
        alias,
        &track_name,
        group_count,
        interval_ms,
        objects_per_group,
        spec,
        priority,
        effective_start,
      )
      .await
    }));
  }

  for task in tasks {
    if let Err(e) = task.await {
      error!("publish-multi: data task error: {:?}", e);
    }
  }

  tokio::time::sleep(Duration::from_secs(2)).await;
  info!("publish-multi: all tracks done, closing connection");
  connection.close(0u32.into(), b"Done");
  Ok(())
}

/// Send PUBLISH and wait for PublishOk; does NOT send any data.
async fn send_publish_header(
  control_stream: &mut ControlStreamHandler,
  namespace: &Tuple,
  track_name: &str,
  track_alias: u64,
  group_order: GroupOrder,
  publisher_priority: u8,
) -> Result<()> {
  let _ = publisher_priority; // priority is set per-stream in SubgroupHeader, not in PUBLISH
  let publish = Publish::new(
    track_alias - 1, // request_id = alias - 1 (0-based)
    namespace.clone(),
    TupleField::from_utf8(track_name),
    track_alias,
    vec![
      MessageParameter::new_group_order(group_order),
      MessageParameter::new_largest_object(Location::new(0, 0)),
      MessageParameter::Forward { forward: true },
    ],
    vec![],
  );
  control_stream
    .send(&ControlMessage::Publish(Box::new(publish)))
    .await?;

  match control_stream.next_message().await {
    Ok(ControlMessage::PublishOk(m)) => {
      info!(
        "PublishOk: request_id={} track='{}' alias={}",
        m.request_id, track_name, track_alias
      );
      Ok(())
    }
    Ok(m) => anyhow::bail!("Expected PublishOk for '{}', got {:?}", track_name, m),
    Err(e) => anyhow::bail!("Failed waiting for PublishOk for '{}': {:?}", track_name, e),
  }
}

/// Synchronized per-track data sender.
///
/// Each object is scheduled to depart at:
///   `start + interval * (group * objects_per_group + object)`
///
/// All tracks share the same `start` instant, so they advance their group
/// counters in lock-step.
async fn send_multi_track(
  connection: &Arc<wtransport::Connection>,
  track_alias: u64,
  track_name: &str,
  group_count: u64,
  interval_ms: u64,
  objects_per_group: u64,
  spec: TrackSpec,
  publisher_priority: u8,
  start: tokio::time::Instant,
) -> Result<()> {
  let interval = Duration::from_millis(interval_ms);
  let (i_size, p_size) = spec.gop_sizes(objects_per_group);
  info!(
    "publish-multi track='{}' alias={}: {} groups × {} objects, \
     I={}B P={}B avg={}B p_ratio={:.2} {}ms interval",
    track_name,
    track_alias,
    group_count,
    objects_per_group,
    i_size,
    p_size,
    spec.payload_size,
    spec.p_ratio,
    interval_ms,
  );

  for group_id in 0..group_count {
    let stream = connection.open_uni().await?.await?;
    let sub_header = SubgroupHeader::new_with_explicit_id(
      track_alias,
      group_id,
      1u64,
      Some(publisher_priority),
      true,
      true,
    );
    let header_info = HeaderInfo::Subgroup { header: sub_header };
    let stream = Arc::new(Mutex::new(stream));
    let mut handler = SendDataStream::new(stream, header_info).await?;

    let mut prev_object_id = None;
    for object_id in 0..objects_per_group {
      // Wait until this object's scheduled slot.
      let seq = group_id * objects_per_group + object_id;
      let due = start + interval.mul_f64(seq as f64);
      tokio::time::sleep_until(due).await;

      // Object 0 of every group is the I-frame; all others are P-frames.
      let frame_size = if object_id == 0 { i_size } else { p_size };
      let payload = generate_payload(frame_size);
      let subgroup_obj = SubgroupObject {
        object_id,
        extension_headers: Some(vec![]),
        object_status: None,
        payload: Some(Bytes::from(payload)),
      };
      let object =
        Object::try_from_subgroup(subgroup_obj, track_alias, group_id, Some(group_id), Some(1))?;

      match handler.send_object(&object, prev_object_id).await {
        Ok(_) => {
          if should_log(seq) {
            let frame_type = if object_id == 0 { "I" } else { "P" };
            info!(
              "publish-multi track='{}': group={} object={} {}={}B",
              track_name, group_id, object_id, frame_type, frame_size
            );
          } else {
            debug!(
              "publish-multi track='{}': group={} object={}",
              track_name, group_id, object_id
            );
          }
        }
        Err(e) => error!(
          "publish-multi track='{}': send_object failed g={} o={}: {:?}",
          track_name, group_id, object_id, e
        ),
      }
      prev_object_id = Some(object_id);
    }

    handler.flush().await?;
  }

  info!("publish-multi track='{}': all groups sent", track_name);
  Ok(())
}
