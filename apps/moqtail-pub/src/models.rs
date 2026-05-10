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

use bytes::Bytes;
// TODO: use moqtail::model::catalog::warp_catalog::Catalog instead
use crate::catalog::Catalog;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TrackType {
  Video,
  Audio,
  Other,
}

impl std::fmt::Display for TrackType {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      TrackType::Video => write!(f, "video"),
      TrackType::Audio => write!(f, "audio"),
      TrackType::Other => write!(f, "other"),
    }
  }
}

pub trait TrackData {
  fn track_name(&self) -> String;
  fn payload(&self) -> Bytes;
}

#[derive(Clone, PartialEq, Debug)]
pub struct FrameData {
  pub track_name: String,
  pub payload: Bytes,
  pub group_id: u64,
  pub object_id: u64,
}

impl TrackData for FrameData {
  fn track_name(&self) -> String {
    self.track_name.clone()
  }
  fn payload(&self) -> Bytes {
    self.payload.clone()
  }
}

impl FrameData {
  pub fn new(track_name: String, payload: Bytes, group_id: u64, object_id: u64) -> Self {
    Self {
      track_name,
      payload,
      group_id,
      object_id,
    }
  }
}

#[derive(Clone, PartialEq, Debug)]
pub enum TrackEvent {
  Keyframe(FrameData),
  Frame(FrameData),
  Catalog(Catalog),
  TimeEvent(TimeEvent),
}

#[derive(Clone, Debug, PartialEq)]
pub struct TimeEvent {
  pub track_id: u32,
  pub ntp_timestamp: u64,
  pub media_time: u64,
  pub keyframe_count: u64,
}

impl TimeEvent {
  pub fn new(track_id: u32, ntp_timestamp: u64, media_time: u64, keyframe_count: u64) -> Self {
    Self {
      track_id,
      ntp_timestamp,
      media_time,
      keyframe_count,
    }
  }
}
