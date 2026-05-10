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

use crate::catalog::Catalog;
use crate::models::{TrackData, TrackEvent};
use crate::timeline::Timeline;
use moqtail::model::common::tuple::Tuple;
use moqtail::model::control::client_setup::ClientSetup;
use moqtail::model::control::control_message::ControlMessage;
use moqtail::model::control::publish_namespace::PublishNamespace;
use moqtail::model::control::subscribe_ok::SubscribeOk;
use moqtail::model::data::constant::ObjectStatus;
use moqtail::model::data::object::Object;
use moqtail::model::data::subgroup_header::SubgroupHeader;
use moqtail::model::data::subgroup_object::SubgroupObject;
use moqtail::transport::control_stream_handler::ControlStreamHandler;
use moqtail::transport::data_stream_handler::{HeaderInfo, SendDataStream};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::{Mutex, RwLock};
use tokio::task::JoinHandle;
use tracing::{debug, error, info};
use wtransport::endpoint::ConnectOptions;
use wtransport::{ClientConfig, Endpoint};

#[derive(Clone)]
pub(crate) struct Client {
  pub endpoint: String,
  control_stream_handler: Option<Arc<Mutex<ControlStreamHandler>>>,
  continue_publishing: Arc<Mutex<bool>>,
  recv: Arc<RwLock<UnboundedReceiver<Vec<TrackEvent>>>>,
  // track name -> sender. the event loop will send the events to the subscriber via this sender
  // when a new subscribe message is received, a new sender will be created and added to the map
  // when a subscriber is unsubscribed, the sender will be removed from the map
  subscribers: Arc<RwLock<BTreeMap<String, UnboundedSender<TrackEvent>>>>,
  catalog: Arc<RwLock<Catalog>>,
  timeline: Arc<RwLock<Timeline>>,
}

impl Client {
  pub fn new(
    endpoint: String,
    _validate_cert: bool,
    recv: UnboundedReceiver<Vec<TrackEvent>>,
  ) -> Self {
    Client {
      endpoint,
      control_stream_handler: None,
      continue_publishing: Arc::new(Mutex::new(true)),
      recv: Arc::new(RwLock::new(recv)),
      subscribers: Arc::new(RwLock::new(BTreeMap::new())),
      catalog: Arc::new(RwLock::new(Catalog::new(vec![]))),
      timeline: Arc::new(RwLock::new(Timeline::new(vec![]))),
    }
  }

  pub async fn run(&mut self) -> Result<(), anyhow::Error> {
    // run the event loop
    self.event_loop();

    let endpoint = self.endpoint.clone();

    let c = ClientConfig::builder().with_bind_default();
    let config = c.with_native_certs().build();

    let options = ConnectOptions::builder(endpoint.as_str())
      .add_header("wt-available-protocols", "\"moqt-16\"")
      .build();

    let connection = match Endpoint::client(config) {
      Ok(client) => match client.connect(options).await {
        Ok(conn) => Arc::new(conn),
        Err(e) => {
          error!("Failed to connect to {}: {:?}", endpoint, e);
          return Err(anyhow::anyhow!("Connection failed: {}", e));
        }
      },
      Err(e) => {
        error!("Failed to create client endpoint: {:?}", e);
        return Err(anyhow::anyhow!("Client creation failed: {}", e));
      }
    };

    let (send_stream, recv_stream) = connection.open_bi().await.unwrap().await.unwrap();

    let mut control_stream_handler = ControlStreamHandler::new(send_stream, recv_stream);

    let client_setup = ClientSetup::new([].to_vec());

    match control_stream_handler.send_impl(&client_setup).await {
      Ok(_) => info!("Client setup sent successfully"),
      Err(e) => error!("Failed to send client setup: {:?}", e),
    }

    let server_setup = match control_stream_handler.next_message().await {
      Ok(ControlMessage::ServerSetup(m)) => m,
      Ok(m) => {
        error!("Unexpected message type: {:?}", m);
        return Err(anyhow::anyhow!("Unexpected message type: {:?}", m));
      }
      Err(e) => {
        error!("Failed to receive server setup: {:?}", e);
        return Err(anyhow::anyhow!("Failed to receive server setup: {:?}", e));
      }
    };

    info!("Received server setup: {:?}", server_setup);

    self.control_stream_handler = Some(Arc::new(Mutex::new(control_stream_handler)));

    self.start_publisher(connection.clone()).await;

    Ok(())
  }

  fn event_loop(&self) -> JoinHandle<()> {
    let recv = self.recv.clone();
    let continue_publishing = self.continue_publishing.clone();
    let subscribers = self.subscribers.clone();
    let catalog = self.catalog.clone();

    tokio::spawn(async move {
      let mut recv = recv.write().await;

      loop {
        if !*continue_publishing.lock().await {
          info!("Stopped publishing, exiting loop");
          break;
        }
        let events = recv.recv().await;
        match events {
          Some(events) => {
            for event in events {
              let tracks = match event.clone() {
                TrackEvent::Keyframe(data) => {
                  info!(
                    "Received keyframe event for track: {} group_id: {}",
                    data.track_name(),
                    data.group_id
                  );

                  // Keyframe event is sent for a video track
                  // So when we receive a keyframe event, we need to send the event
                  // for the video track and all the audio tracks subscribers
                  let video_track = if data.track_name().eq("1") {
                    "video".to_string()
                  } else if data.track_name().eq("2") {
                    "audio".to_string()
                  } else {
                    data.track_name().to_string()
                  };

                  // find the audio track names from the catalog
                  let catalog = catalog.read().await;
                  let mut tracks = catalog
                    .tracks
                    .iter()
                    .filter(|t| t.role == "audio")
                    .map(|t| t.track_name.clone())
                    .collect::<Vec<String>>();
                  tracks.push(video_track);
                  tracks
                }
                TrackEvent::Frame(data) => {
                  vec![data.track_name().to_string()]
                }
                TrackEvent::Catalog(cat) => {
                  info!("Received catalog: {:?}", cat.to_string());
                  let mut catalog = catalog.write().await;
                  *catalog = cat.clone();
                  vec![]
                }
                TrackEvent::TimeEvent(time_event) => {
                  info!("Received time event: {:?}", time_event);
                  vec![]
                }
              };

              if tracks.is_empty() {
                continue;
              }

              // send the event to the subscribers
              for track_name in tracks {
                let track_name = if track_name.eq("1") {
                  "video".to_string()
                } else if track_name.eq("2") {
                  "audio".to_string()
                } else {
                  track_name
                };
                // find the sender for the track
                let subscribers = subscribers.read().await;
                let sender = match subscribers.get(&track_name) {
                  Some(sender) => sender,
                  None => {
                    // error!("No sender found for track: {}", track_name);
                    continue;
                  }
                };
                // send the event to the sender
                let _ = sender.send(event.clone());
              }
            }
          }
          None => {
            info!("No more events");
            break;
          }
        }
      }
    })
  }

  async fn start_publisher(&self, connection: Arc<wtransport::Connection>) {
    info!("Starting publisher...");

    let my_namespace = Tuple::from_utf8_path("moqtail-watch-party-live");

    let request_id = 0;

    // start by sending an announce message
    self
      .send_announce_and_wait(request_id, my_namespace.clone())
      .await
      .unwrap();

    info!("Publish namespace sent successfully");

    let catalog_track_alias = 101u64;
    let timeline_track_alias = 102u64;
    let video_track_alias = 103u64;
    let audio_track_alias = 104u64;

    // wait for subscribe or fetch, enter loop
    loop {
      let message;
      {
        let control_stream_handler = self.control_stream_handler.clone().unwrap();
        let mut control = control_stream_handler.lock().await;
        message = control.next_message().await;
      }

      match message {
        Ok(ControlMessage::Subscribe(m)) => {
          info!("Received subscribe message: {:?}", m);

          // send Subscribe_ok
          let track_alias = match m.track_name.as_str().as_str() {
            "catalog" => catalog_track_alias,
            "timeline" => timeline_track_alias,
            "video" => video_track_alias,
            "audio" => audio_track_alias,
            _ => {
              error!("Unknown track name: {}", m.track_name);
              continue;
            }
          };
          self.send_subscribe_ok(m.request_id, track_alias).await;

          // create a new sender for the track
          let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<TrackEvent>();
          let sender = tx;
          // TODO: use a bounded channel instead of unbounded
          self
            .subscribers
            .write()
            .await
            .insert(m.track_name.as_str(), sender);

          info!("Created new subscriber for track: {}", m.track_name.clone());

          // open a unidirectional stream
          let connection = connection.clone();
          let continue_publishing = self.continue_publishing.clone();
          let catalog = self.catalog.clone();
          let timeline = self.timeline.clone();

          tokio::spawn(async move {
            let mut recv = rx;

            let mut group_id = 0;
            let mut object_id = 0;
            let mut stream_handler: Option<SendDataStream> = None;

            info!("Starting to publish for track_name: {}", m.track_name);

            if m.track_name.as_str().as_str().eq("catalog") {
              loop {
                object_id = 1;
                group_id += 1;
                debug!(
                  "Sending catalog to subscriber group_id: {} object_id: {}",
                  group_id, object_id
                );
                let stream = connection.open_uni().await.unwrap().await.unwrap();
                let sub_header = SubgroupHeader::new_with_explicit_id(
                  catalog_track_alias,
                  group_id,
                  1,
                  Some(1),
                  false,
                  false,
                );

                let header_info = HeaderInfo::Subgroup {
                  header: sub_header,
                  /*subscribe_request: *m.clone(),*/
                };
                let stream = Arc::new(Mutex::new(stream));
                stream_handler = Some(
                  SendDataStream::new(stream.clone(), header_info)
                    .await
                    .unwrap(),
                );
                // catalog object is the first object in the stream
                let catalog = catalog.read().await;
                let object = SubgroupObject {
                  object_id,
                  extension_headers: None,
                  object_status: Some(ObjectStatus::Normal),
                  payload: Some(catalog.payload().clone()),
                };

                let object = Object::try_from_subgroup(
                  object,
                  catalog_track_alias,
                  group_id,
                  Some(1u64),
                  Some(1),
                )
                .unwrap();

                // send the object
                if let Some(ref mut handler) = stream_handler {
                  handler.send_object(&object, None).await.unwrap();
                  handler.flush().await.unwrap();
                  debug!(
                    "Flushing and finishing catalog stream for group_id: {}",
                    group_id
                  );
                  handler.finish().await.unwrap();
                }

                tokio::time::sleep(Duration::from_secs(1)).await;
              }
            }

            if m.track_name.as_str().as_str().eq("timeline") {
              loop {
                object_id = 1;
                group_id += 1;
                debug!(
                  "Sending timeline to subscriber group_id: {} object_id: {}",
                  group_id, object_id
                );
                let stream = connection.open_uni().await.unwrap().await.unwrap();
                let sub_header = SubgroupHeader::new_with_explicit_id(
                  timeline_track_alias,
                  group_id,
                  1,
                  Some(1),
                  false,
                  false,
                );

                let header_info = HeaderInfo::Subgroup {
                  header: sub_header,
                  /*subscribe_request: *m.clone(),*/
                };
                let stream = Arc::new(Mutex::new(stream));
                stream_handler = Some(
                  SendDataStream::new(stream.clone(), header_info)
                    .await
                    .unwrap(),
                );
                // timeline object is the first object in the stream
                let timeline = timeline.read().await;
                let object = SubgroupObject {
                  object_id,
                  extension_headers: None,
                  object_status: Some(ObjectStatus::Normal),
                  payload: Some(timeline.payload().clone()),
                };

                let object = Object::try_from_subgroup(
                  object,
                  timeline_track_alias,
                  group_id,
                  Some(1u64),
                  Some(1),
                )
                .unwrap();

                // send the object
                if let Some(ref mut handler) = stream_handler {
                  handler.send_object(&object, None).await.unwrap();
                  handler.flush().await.unwrap();
                  debug!(
                    "Flushing and finishing timeline stream for group_id: {}",
                    group_id
                  );
                  handler.finish().await.unwrap();
                }

                tokio::time::sleep(Duration::from_secs(1)).await;
              }
            }

            loop {
              if !*continue_publishing.lock().await {
                info!("Stopped publishing, exiting loop");
                break;
              }

              info!("Waiting for events for track_name: {}", m.track_name);
              let event = recv.recv().await;
              info!("Received event for track_name: {}", m.track_name);

              if event.is_none() {
                info!("No more events");
                break;
              }
              let event = event.unwrap();

              match &event {
                TrackEvent::Keyframe(frame) => {
                  // close the previous stream if any
                  if let Some(mut handler) = stream_handler.take() {
                    let track_name = m.track_name.clone();

                    thread::spawn(async move || {
                      info!(
                        "Finishing stream for track_name: {} group_id: {}",
                        track_name, group_id
                      );
                      handler.finish().await.unwrap();
                    });
                  }

                  info!(
                    "Received keyframe, creating new stream for track_name: {} group_id: {}",
                    m.track_name, frame.group_id
                  );

                  let stream = connection.open_uni().await.unwrap().await.unwrap();
                  let sub_header = SubgroupHeader::new_with_explicit_id(
                    track_alias,
                    frame.group_id,
                    1,
                    Some(1),
                    false,
                    false,
                  );

                  let header_info = HeaderInfo::Subgroup {
                    header: sub_header,
                    /*subscribe_request: *m.clone(),*/
                  };
                  let stream = Arc::new(Mutex::new(stream));
                  stream_handler = Some(
                    SendDataStream::new(stream.clone(), header_info)
                      .await
                      .unwrap(),
                  );

                  object_id = 0;
                }
                TrackEvent::Frame(frame) => {
                  debug!(
                    "Sending frame for track_name: {} group id: {} - object_id: {}",
                    m.track_name, frame.group_id, frame.object_id
                  );
                  // create a new object
                  let object = SubgroupObject {
                    object_id: frame.object_id,
                    extension_headers: None,
                    object_status: Some(ObjectStatus::Normal),
                    payload: Some(frame.payload.clone()),
                  };

                  object_id += 1;
                  let prev_object_id = if object_id > 1 {
                    Some(object_id - 1)
                  } else {
                    None
                  };

                  let object = Object::try_from_subgroup(
                    object,
                    track_alias,
                    frame.group_id,
                    Some(1u64),
                    Some(1),
                  )
                  .unwrap();
                  // send the object
                  if let Some(ref mut handler) = stream_handler {
                    handler.send_object(&object, prev_object_id).await.unwrap();
                  }
                }
                _ => {
                  info!("No more events");
                  break;
                }
              }
            }
          });

          info!("Subscribe ok sent successfully");
        }
        Ok(ControlMessage::Unsubscribe(m)) => {
          info!("Received unsubscribe message: {:?}", m);
          // stop publishing
          let mut continue_publishing = self.continue_publishing.lock().await;
          *continue_publishing = false;
          info!("Stopped publishing");
        }
        Ok(_) => {
          error!("Unexpected message type");
        }
        Err(e) => {
          error!("Failed to receive message: {:?}", e);
          break;
        }
      }
    }
  }

  async fn send_subscribe_ok(&self, request_id: u64, track_alias: u64) {
    info!("Sending SubscribeOk message");
    let control_stream_handler = self.control_stream_handler.clone().unwrap();
    let mut control_stream_handler = control_stream_handler.lock().await;
    info!("Control stream handler locked");
    let msg = SubscribeOk::new(request_id, track_alias, [].to_vec(), [].to_vec());
    control_stream_handler.send_impl(&msg).await.unwrap();
    info!("SubscribeOk message sent successfully");
  }

  async fn send_announce_and_wait(
    &self,
    request_id: u64,
    my_namespace: Tuple,
  ) -> Result<(), anyhow::Error> {
    let control_stream_handler = self.control_stream_handler.clone().unwrap();
    let mut control_stream_handler = control_stream_handler.lock().await;
    // send announce, request id 0
    let announce = PublishNamespace::new(request_id, my_namespace, &[]);
    control_stream_handler.send_impl(&announce).await.unwrap();

    let announce_ok = control_stream_handler.next_message().await;
    match announce_ok {
      Ok(ControlMessage::RequestOk(m)) => {
        info!("Received request ok message: {:?}", m);
        Ok(())
      }
      Ok(_) => {
        error!("Expecting publish namespace ok message");
        Err(anyhow::anyhow!("Expecting publish namespace ok message"))
      }
      Err(e) => {
        // TODO: request id mismatch should be handled in control stream handler
        error!("Failed to receive message: {:?}", e);
        Err(anyhow::anyhow!("Failed to receive message: {:?}", e))
      }
    }
  }
}
