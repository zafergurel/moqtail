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

mod cli;
mod connection;
mod fetcher;
mod filter_spec;
mod prefix_subscriber;
mod publisher;
mod routing;
mod stats;
mod subscriber;
mod switcher;
mod utils;

use clap::Parser;
use cli::{Cli, CliFilter, Command, PrefixKind};
use connection::MoqConnection;
use moqtail::model::parameter::message_parameter::MessageParameter;
use publisher::TrackSpec;
use tracing::info;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::LevelFilter;

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
  init_logging();

  let cli = Cli::parse();

  info!(
    "Starting moqtail client: server={}, namespace={}, track={}",
    cli.server, cli.namespace, cli.track_name
  );

  let moq_conn =
    MoqConnection::establish(&cli.server, cli.no_cert_validation, cli.transport).await?;

  let content = publisher::Content {
    subgroups_per_group: cli.subgroups_per_group,
    priorities: cli.priorities.clone(),
    object_properties: cli.object_property.clone(),
    track_properties: cli.track_property.clone(),
    fetch_markers: cli.fetch_end_of_range.clone(),
    fetch_reset_after: cli.fetch_reset_after,
    end_of_group_objects: cli.end_of_group_objects,
    end_of_track_object: cli.end_of_track_object,
    reset_streams_at: cli.reset_stream_at.clone(),
    dropped_datagrams: cli.drop_datagram.clone(),
    gap_properties: !cli.no_gap_properties,
  };

  match cli.command {
    Command::Publish => {
      let config = publisher::PublishConfig {
        namespace: cli.namespace,
        track_name: cli.track_name,
        delivery_mode: cli.delivery_mode,
        group_count: cli.group_count,
        pre_published_groups: cli.pre_published_groups,
        interval: cli.interval,
        objects_per_group: cli.objects_per_group,
        object_id_step: cli.object_id_step,
        group_id_step: cli.group_id_step,
        payload_size: cli.payload_size,
        track_alias: cli
          .track_alias
          .unwrap_or_else(|| rand::random::<u64>() & ((1u64 << 62) - 1)),
        publisher_priority: cli.publisher_priority,
        content: content.clone(),
      };
      publisher::run(moq_conn, config).await
    }

    Command::PublishNamespace => {
      let config = publisher::PublishNamespaceConfig {
        namespace: cli.namespace,
        withdraw_after: cli.withdraw_after,
        delivery_mode: cli.delivery_mode,
        group_count: cli.group_count,
        pre_published_groups: cli.pre_published_groups,
        interval: cli.interval,
        objects_per_group: cli.objects_per_group,
        object_id_step: cli.object_id_step,
        group_id_step: cli.group_id_step,
        payload_size: cli.payload_size,
        publisher_priority: cli.publisher_priority,
        content: content.clone(),
      };
      publisher::run_namespace(moq_conn, config).await
    }

    Command::PublishMulti => {
      // Parse "name:bytes,name:bytes,..." from --tracks.
      let tracks = parse_tracks(&cli.tracks)?;
      let config = publisher::PublishMultiConfig {
        namespace: cli.namespace,
        tracks,
        objects_per_group: cli.objects_per_group,
        interval_ms: cli.interval,
        group_count: cli.group_count,
        publisher_priority: cli.publisher_priority,
      };
      publisher::run_multi(moq_conn, config).await
    }

    Command::Subscribe => {
      let config = subscriber::SubscribeConfig {
        namespace: cli.namespace,
        track_name: cli.track_name,
        delivery_mode: cli.delivery_mode,
        duration: cli.duration,
        subscriber_priority: cli.subscriber_priority,
        group_order: cli.group_order.into(),
        extra_track: cli.extra_track.as_deref().and_then(|s| {
          let (name, prio) = s.rsplit_once(':')?;
          let priority: u8 = prio.parse().ok()?;
          Some((name.to_string(), priority))
        }),
        forward: cli.forward,
        include_properties: cli.include_properties,
        rendezvous_timeout: cli.rendezvous_timeout,
        update_forward_after: cli.update_forward_after,
        filter: cli.filter.unwrap_or(CliFilter::NextObject),
        filter_start_group: cli.filter_start_group,
        filter_start_object: cli.filter_start_object,
        end_group_delta: cli.end_group_delta,
        filter_end_object: cli.filter_end_object,
        fill: cli.fill || cli.fill_start_group.is_some() || !cli.fill_range_filter.is_empty(),
        fill_start_group: cli.fill_start_group,
        switch_after: cli.switch_after,
        switch_track: cli.switch_track,
        switch_mode: cli.switch_mode.into(),
        switch_publish_done: cli.switch_publish_done,
        switch_start_group: cli.switch_start_group,
        range_filters: cli.range_filter,
        fill_range_filters: cli.fill_range_filter,
        update_filters_after: cli.update_filters_after,
        update_range_filters: cli.update_range_filter,
        ignore_filter_limit: cli.ignore_filter_limit,
      };
      subscriber::run(moq_conn, config).await
    }

    Command::SubscribeNamespace | Command::SubscribeTracks => {
      let config = prefix_subscriber::PrefixSubscribeConfig {
        namespace: cli.namespace,
        duration: cli.duration,
        kind: match cli.command {
          Command::SubscribeNamespace => PrefixKind::Namespace,
          _ => PrefixKind::Tracks,
        },
        overlap: cli.overlap,
        include_properties: cli.include_properties,
        pushed_track_parameters: {
          // Each pushed track's Forward State, where it starts, and a fill of what it
          // already has.
          let mut params = Vec::new();
          if !cli.forward {
            params.push(MessageParameter::new_forward(false));
          }
          if let Some(filter) = cli.filter {
            params.push(MessageParameter::new_location_filter(
              subscriber::cli_location_filter(
                filter,
                cli.filter_start_group,
                cli.filter_start_object,
                cli.end_group_delta,
                cli.filter_end_object,
              ),
            ));
          }
          if cli.fill || cli.fill_start_group.is_some() || !cli.fill_range_filter.is_empty() {
            params.push(subscriber::fill_parameter(
              cli.fill_start_group,
              &cli.fill_range_filter,
            ));
          }
          params
        },
        refuse_publish: cli.refuse_publish,
        range_filters: cli.range_filter,
        ignore_filter_limit: cli.ignore_filter_limit,
      };
      prefix_subscriber::run(moq_conn, config).await
    }
    Command::Fetch => {
      let config = fetcher::FetchConfig {
        namespace: cli.namespace,
        track_name: cli.track_name,
        start_group: cli.start_group,
        start_object: cli.start_object,
        end_group: cli.end_group,
        end_object: cli.end_object,
        group_order: cli.group_order.into(),
        cancel_after: cli.cancel_after,
        include_properties: cli.include_properties,
        range_filters: cli.range_filter,
        ignore_filter_limit: cli.ignore_filter_limit,
      };
      fetcher::run(moq_conn, config).await
    }

    Command::SwitchTest => {
      use crate::stats::PlayoutConfig;

      // Parse optional --track-sequence "2,3,4" into a Vec<String>.
      let track_sequence: Vec<String> = if cli.track_sequence.is_empty() {
        vec![]
      } else {
        cli
          .track_sequence
          .split(',')
          .map(|s| s.trim().to_string())
          .filter(|s| !s.is_empty())
          .collect()
      };

      let playout = PlayoutConfig {
        frame_interval_ms: cli.interval,
        objects_per_group: cli.objects_per_group,
        jitter_buffer_ms: cli.jitter_buffer_ms,
      };

      let config = switcher::SwitchTestConfig {
        namespace: cli.namespace,
        track_sequence,
        track_a: cli.track_a,
        track_b: cli.track_b,
        method: cli.method.into(),
        switch_mode: cli.switch_mode.into(),
        switch_after_ms: cli.switch_after_ms,
        bandwidth_cap_bps: cli.bandwidth_cap_bps,
        output_json: cli.output_json,
        playout,
      };
      switcher::run(moq_conn, config).await
    }
  }
}

/// Parse the `--tracks` string.
///
/// Each entry is either `"name:bytes"` or `"name:bytes:p_ratio"`.
/// `p_ratio` is the P-frame size as a fraction of the I-frame size (default 0.25).
fn parse_tracks(s: &str) -> Result<Vec<TrackSpec>, anyhow::Error> {
  s.split(',')
    .map(|entry| {
      let entry = entry.trim();
      let parts: Vec<&str> = entry.splitn(4, ':').collect();
      match parts.as_slice() {
        [name, bytes_str] => {
          let bytes: usize = bytes_str
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid payload size '{}' in '{}'", bytes_str, entry))?;
          Ok(TrackSpec::new(name.trim(), bytes))
        }
        [name, bytes_str, ratio_str] => {
          let bytes: usize = bytes_str
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid payload size '{}' in '{}'", bytes_str, entry))?;
          let p_ratio: f64 = ratio_str
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid p_ratio '{}' in '{}'", ratio_str, entry))?;
          if p_ratio <= 0.0 || p_ratio > 1.0 {
            anyhow::bail!("p_ratio must be in (0, 1], got {} in '{}'", p_ratio, entry);
          }
          Ok(TrackSpec { name: name.trim().to_string(), payload_size: bytes, p_ratio, start_delay_ms: 0 })
        }
        [name, bytes_str, ratio_str, delay_str] => {
          let bytes: usize = bytes_str
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid payload size '{}' in '{}'", bytes_str, entry))?;
          let p_ratio: f64 = ratio_str
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid p_ratio '{}' in '{}'", ratio_str, entry))?;
          if p_ratio <= 0.0 || p_ratio > 1.0 {
            anyhow::bail!("p_ratio must be in (0, 1], got {} in '{}'", p_ratio, entry);
          }
          let start_delay_ms: u64 = delay_str
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid start_delay_ms '{}' in '{}'", delay_str, entry))?;
          Ok(TrackSpec { name: name.trim().to_string(), payload_size: bytes, p_ratio, start_delay_ms })
        }
        _ => anyhow::bail!(
          "invalid track spec '{}'; expected 'name:bytes', 'name:bytes:p_ratio', or 'name:bytes:p_ratio:delay_ms'",
          entry
        ),
      }
    })
    .collect()
}

fn init_logging() {
  let env_filter = EnvFilter::builder()
    .with_default_directive(LevelFilter::INFO.into())
    .from_env_lossy();

  tracing_subscriber::fmt()
    .with_target(true)
    .with_level(true)
    .with_env_filter(env_filter)
    .init();
}
