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

use crate::switcher::SwitchMethod;
use clap::{Parser, ValueEnum};
use moqtail::model::control::constant::GroupOrder;

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum CliGroupOrder {
  Original,
  Ascending,
  Descending,
}

impl From<CliGroupOrder> for GroupOrder {
  fn from(o: CliGroupOrder) -> Self {
    match o {
      CliGroupOrder::Original => GroupOrder::Original,
      CliGroupOrder::Ascending => GroupOrder::Ascending,
      CliGroupOrder::Descending => GroupOrder::Descending,
    }
  }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum DeliveryMode {
  /// Send/receive objects via unidirectional streams with subgroup headers
  Subgroup,
  /// Send/receive objects via QUIC datagrams
  Datagram,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum CliSwitchMethod {
  /// Relay-executed atomic switch via SWITCH message (1 control message)
  SwitchMessage,
  /// Pre-subscribe B; enable B at timer fire, tear down A on first live B group boundary (3 control messages)
  SubUpdateForward,
  /// Joining Fetch warm-up then stop A on first live B object (4–5 control messages)
  JoiningFetch,
}

impl From<CliSwitchMethod> for SwitchMethod {
  fn from(m: CliSwitchMethod) -> Self {
    match m {
      CliSwitchMethod::SwitchMessage => SwitchMethod::SwitchMessage,
      CliSwitchMethod::SubUpdateForward => SwitchMethod::SubUpdateForward,
      CliSwitchMethod::JoiningFetch => SwitchMethod::JoiningFetch,
    }
  }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Command {
  /// Publish objects to a track
  Publish,
  /// Publish a namespace and auto-respond to subscribes with test data
  PublishNamespace,
  /// Publish multiple tracks simultaneously in push mode (for switch-test)
  PublishMulti,
  /// Subscribe to a track and receive objects
  Subscribe,
  /// Fetch specific object ranges from a track
  Fetch,
  /// Run a track-switching experiment and emit JSON stats
  SwitchTest,
}

#[derive(Parser, Debug)]
#[command(
  name = "moqtail-client",
  author,
  version,
  about = "MOQtail test client"
)]
pub struct Cli {
  /// Command to run
  #[arg(long, short, value_enum)]
  pub command: Command,

  /// Server address
  #[arg(long, short, default_value = "https://127.0.0.1:4433")]
  pub server: String,

  /// Track namespace
  #[arg(long, short, default_value = "moqtail")]
  pub namespace: String,

  /// Track name (publish / subscribe / fetch only)
  #[arg(long, short = 'T', default_value = "demo")]
  pub track_name: String,

  /// Skip certificate validation (for testing with self-signed certs)
  #[arg(long, default_value_t = false)]
  pub no_cert_validation: bool,

  /// Delivery mode: how objects are sent/received (subgroup streams or QUIC datagrams)
  #[arg(long, value_enum, default_value = "subgroup")]
  pub delivery_mode: DeliveryMode,

  /// Number of groups to send (publish / publish-multi only)
  #[arg(long, default_value_t = 1000)]
  pub group_count: u64,

  /// Interval between objects in milliseconds (publish / publish-multi only)
  #[arg(long, short, default_value_t = 40)]
  pub interval: u64,

  /// Number of objects per group (publish / publish-multi only)
  #[arg(long, default_value_t = 25)]
  pub objects_per_group: u64,

  /// Payload size in bytes (publish only — ignored for publish-multi)
  #[arg(long, default_value_t = 1200)]
  pub payload_size: usize,

  /// Track alias (publish only, random if not specified)
  #[arg(long)]
  pub track_alias: Option<u64>,

  /// Duration to listen in seconds, 0 = indefinite (subscribe only)
  #[arg(long, short, default_value_t = 0)]
  pub duration: u64,

  /// Start group ID (fetch only)
  #[arg(long, default_value_t = 1)]
  pub start_group: u64,

  /// Start object ID (fetch only)
  #[arg(long, default_value_t = 0)]
  pub start_object: u64,

  /// End group ID (fetch only)
  #[arg(long, default_value_t = 5)]
  pub end_group: u64,

  /// End object ID (fetch only)
  #[arg(long, default_value_t = 3)]
  pub end_object: u64,

  /// Cancel the fetch after receiving N objects (fetch only, 0 = no cancel)
  #[arg(long, default_value_t = 0)]
  pub cancel_after: u64,

  /// Subscriber priority 0 (highest) – 255 (lowest) (subscribe only)
  #[arg(long, default_value_t = 128)]
  pub subscriber_priority: u8,

  /// Publisher priority 0 (highest) – 255 (lowest) (publish / publish-multi)
  #[arg(long, default_value_t = 128)]
  pub publisher_priority: u8,

  /// Group order for the track
  #[arg(long, value_enum, default_value = "ascending")]
  pub group_order: CliGroupOrder,

  /// Additional track to subscribe to for priority testing: "track-name:priority"
  /// e.g. --extra-track demo2:200
  #[arg(long)]
  pub extra_track: Option<String>,

  // ── publish-multi args ────────────────────────────────────────────────────
  /// Comma-separated track specs: "name:avg_bytes[:p_ratio[:delay_ms]]".
  /// avg_bytes  — average payload per object (controls bitrate).
  /// p_ratio    — P-frame size as fraction of I-frame; default 0.25.
  /// delay_ms   — ms to delay before this track starts publishing; default 0.
  ///              Use to create a controlled intra-GOP phase offset between tracks.
  /// Track numbering: lower number = lower bitrate (track 1 is lowest quality).
  ///   1:2500,2:5000,3:12500,4:20000
  /// Example with offsets: --tracks "3:12500:0.25:0,4:20000:0.25:500"
  #[arg(long, default_value = "1:2500,2:5000,3:12500,4:20000")]
  pub tracks: String,

  // ── switch-test args ──────────────────────────────────────────────────────
  /// Track to subscribe to initially (single-switch convenience; ignored when
  /// --track-sequence is given)
  #[arg(long, default_value = "2")]
  pub track_a: String,

  /// Track to switch to (single-switch convenience; ignored when
  /// --track-sequence is given)
  #[arg(long, default_value = "3")]
  pub track_b: String,

  /// Comma-separated ordered list of track names for a multi-switch run.
  /// When provided, overrides --track-a / --track-b.
  /// Example: --track-sequence "2,3,4"
  #[arg(long, default_value = "")]
  pub track_sequence: String,

  /// Switch method to use
  #[arg(long, value_enum, default_value = "switch-message")]
  pub method: CliSwitchMethod,

  /// Seconds to receive each track before triggering the next switch
  #[arg(long, default_value_t = 15)]
  pub switch_after: u64,

  /// Bandwidth cap in bps recorded in the output JSON (0 = no limit)
  #[arg(long, default_value_t = 0)]
  pub bandwidth_cap_bps: u64,

  /// Write SwitchStats as JSON to this file
  #[arg(long)]
  pub output_json: Option<String>,

  /// Jitter buffer target in ms. Frames arriving more than this many ms past
  /// their expected playout time are dropped, causing a stall of at least one GoP.
  #[arg(long, default_value_t = 0)]
  pub jitter_buffer_ms: u64,
}
