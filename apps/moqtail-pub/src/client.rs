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
use moqtail::model::control::control_message::{ControlMessage, ControlMessageTrait};
use moqtail::model::control::publish_namespace::PublishNamespace;
use moqtail::model::control::setup::Setup;
use moqtail::model::control::subscribe_ok::SubscribeOk;
use moqtail::model::data::constant::ObjectStatus;
use moqtail::model::data::object::Object;
use moqtail::model::data::subgroup_header::SubgroupHeader;
use moqtail::model::data::subgroup_object::SubgroupObject;
use moqtail::transport::connection::TransportConnection;
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
      .add_header("wt-available-protocols", "\"moqt-22\"")
      .build();

    let connection = match Endpoint::client(config) {
      Ok(client) => match client.connect(options).await {
        Ok(conn) => Arc::new(TransportConnection::WebTransport(conn)),
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

    // The control plane is a pair of unidirectional streams: send CLIENT_SETUP
    // on ours, then read the server's SETUP from its.
    let mut send_stream = connection.open_uni().await?;
    let setup_bytes = Setup::new(vec![])
      .serialize()
      .map_err(|e| anyhow::anyhow!("Failed to serialize CLIENT_SETUP: {e:?}"))?;
    send_stream
      .write_all(&setup_bytes)
      .await
      .map_err(|e| anyhow::anyhow!("Failed to send CLIENT_SETUP: {e:?}"))?;
    info!("Client setup sent successfully");

    let recv_stream = connection.accept_uni().await?;
    let mut control_stream_handler = ControlStreamHandler::new(send_stream, recv_stream);
    let server_setup = control_stream_handler
      .read_setup()
      .await
      .map_err(|e| anyhow::anyhow!("Failed to receive server setup: {:?}", e))?;
    info!("Received server setup: {:?}", server_setup);

    // Held for the session's lifetime; requests use their own bidi streams.
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
                  let video_track = data.track_name();

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

              // send the event to the subscribers of the track, by its MP4 track
              // id ("1", "2", ...) and, for tracks 1 and 2, by the name the
              // single-track watch-party layout gives them ("video", "audio")
              let subscribers = subscribers.read().await;
              for track_name in tracks {
                let alias = match track_name.as_str() {
                  "1" => Some("video"),
                  "2" => Some("audio"),
                  _ => None,
                };
                for name in std::iter::once(track_name.as_str()).chain(alias) {
                  if let Some(sender) = subscribers.get(name) {
                    let _ = sender.send(event.clone());
                  }
                }
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

  async fn start_publisher(&self, connection: Arc<TransportConnection>) {
    info!("Starting publisher...");

    let my_namespace = Tuple::from_utf8_path("moqtail-watch-party-live");

    let request_id = 0;

    // start by sending an announce message; its request stream stays open for
    // as long as the namespace is published
    let _namespace_stream = self
      .send_announce_and_wait(&connection, request_id, my_namespace.clone())
      .await
      .unwrap();

    info!("Publish namespace sent successfully");

    let catalog_track_alias = 101u64;
    let timeline_track_alias = 102u64;
    let video_track_alias = 103u64;
    let audio_track_alias = 104u64;

    // each request (SUBSCRIBE) arrives on its own bidi stream
    loop {
      let (send, recv) = match connection.accept_bi().await {
        Ok(streams) => streams,
        Err(e) => {
          error!("Request stream accept ended: {:?}", e);
          break;
        }
      };
      let this = self.clone();
      let connection = connection.clone();
      tokio::spawn(async move {
        let mut request_stream = ControlStreamHandler::new(send, recv);
        this
          .serve_request(
            &mut request_stream,
            connection,
            [
              catalog_track_alias,
              timeline_track_alias,
              video_track_alias,
              audio_track_alias,
            ],
          )
          .await;
      });
    }
  }

  async fn serve_request(
    &self,
    request_stream: &mut ControlStreamHandler,
    connection: Arc<TransportConnection>,
    [
      catalog_track_alias,
      timeline_track_alias,
      video_track_alias,
      audio_track_alias,
    ]: [u64; 4],
  ) {
    let message = request_stream.next_message().await;
    {
      match message {
        Ok(ControlMessage::Subscribe(m)) => {
          info!("Received subscribe message: {:?}", m);

          // send Subscribe_ok
          let track_alias = match m.track_name.as_str().as_str() {
            "catalog" => catalog_track_alias,
            "timeline" => timeline_track_alias,
            "video" => video_track_alias,
            "audio" => audio_track_alias,
            // an MP4 track id, as the multi-track ABR ladder is subscribed to
            other => match other.parse::<u64>() {
              Ok(n) => 200 + n,
              Err(_) => {
                error!("Unknown track name: {}", m.track_name);
                return;
              }
            },
          };
          if let Err(e) = request_stream
            .send_impl(&SubscribeOk::new(track_alias, vec![], vec![]))
            .await
          {
            error!("Failed to send SubscribeOk: {:?}", e);
            return;
          }

          // create a new sender for the track
          let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<TrackEvent>();
          let sender = tx.clone();
          let track_name = m.track_name.as_str();
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
            let mut object_id;
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
                let stream = connection.open_uni().await.unwrap();
                let sub_header = SubgroupHeader::new_with_explicit_id(
                  catalog_track_alias,
                  group_id,
                  1,
                  Some(1),
                  false,
                  false,
                  true,
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
                  properties: None,
                  object_status: Some(ObjectStatus::Normal),
                  payload: Some(catalog.payload().clone()),
                };

                let object = Object::try_from_subgroup(
                  object,
                  catalog_track_alias,
                  group_id,
                  Some(1u64),
                  Some(1),
                  1,
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
                let stream = connection.open_uni().await.unwrap();
                let sub_header = SubgroupHeader::new_with_explicit_id(
                  timeline_track_alias,
                  group_id,
                  1,
                  Some(1),
                  false,
                  false,
                  true,
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
                  properties: None,
                  object_status: Some(ObjectStatus::Normal),
                  payload: Some(timeline.payload().clone()),
                };

                let object = Object::try_from_subgroup(
                  object,
                  timeline_track_alias,
                  group_id,
                  Some(1u64),
                  Some(1),
                  1,
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

                  let stream = connection.open_uni().await.unwrap();
                  let sub_header = SubgroupHeader::new_with_explicit_id(
                    track_alias,
                    frame.group_id,
                    1,
                    Some(1),
                    false,
                    false,
                    true,
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

                }
                TrackEvent::Frame(frame) => {
                  debug!(
                    "Sending frame for track_name: {} group id: {} - object_id: {}",
                    m.track_name, frame.group_id, frame.object_id
                  );
                  // create a new object
                  let object = SubgroupObject {
                    object_id: frame.object_id,
                    properties: None,
                    object_status: Some(ObjectStatus::Normal),
                    payload: Some(frame.payload.clone()),
                  };

                  let prev_object_id = frame.object_id.checked_sub(1);

                  let object = Object::try_from_subgroup(
                    object,
                    track_alias,
                    frame.group_id,
                    Some(1u64),
                    Some(1),
                    1,
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

          // The subscriber cancels by ending the request stream (UNSUBSCRIBE is
          // gone): stop feeding this track, unless a newer subscription took it.
          while request_stream.next_message().await.is_ok() {}
          info!("Subscription for track {} ended", track_name);
          let mut subscribers = self.subscribers.write().await;
          if subscribers
            .get(&track_name)
            .is_some_and(|s| s.same_channel(&tx))
          {
            subscribers.remove(&track_name);
          }
        }
        Ok(m) => {
          error!("Unexpected message on request stream: {:?}", m);
        }
        Err(e) => {
          error!("Failed to receive message: {:?}", e);
        }
      }
    }
  }

  async fn send_announce_and_wait(
    &self,
    connection: &TransportConnection,
    request_id: u64,
    my_namespace: Tuple,
  ) -> Result<ControlStreamHandler, anyhow::Error> {
    // PUBLISH_NAMESPACE goes on its own request stream; the response returns on it
    let (send, recv) = connection.open_bi().await?;
    let mut request_stream = ControlStreamHandler::new(send, recv);
    let announce = PublishNamespace::new(request_id, my_namespace, &[]);
    request_stream.send_impl(&announce).await.unwrap();

    let announce_ok = request_stream.next_message().await;
    match announce_ok {
      Ok(ControlMessage::RequestOk(m)) => {
        info!("Received request ok message: {:?}", m);
        Ok(request_stream)
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
