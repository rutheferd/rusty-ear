use futures_util::FutureExt;
use rust_socketio::{
    asynchronous::ClientBuilder, Payload
};
use serde_json::json;
use serde_json::Value;
use webrtc::interceptor::registry;
use webrtc::peer_connection;
use webrtc::turn::proto::data;
use std::time::Duration;
use log::{info, error};
use env_logger;
use tokio::sync::Notify;
use std::sync::Arc;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::setting_engine::SettingEngine;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::ice_transport::ice_connection_state::RTCIceConnectionState;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::ice_transport::ice_candidate::RTCIceCandidate;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use webrtc::media::Sample;
use webrtc::interceptor::registry::Registry;
use webrtc::api::APIBuilder;
use webrtc::media::io::sample_builder::SampleBuilder;

// Shared variable for camera stream (similar to Python example)
struct CameraStream {
    track: Arc<TrackLocalStaticSample>,
}

// Initialize WebRTC setup, including ICE servers
async fn setup_webrtc(room: &str, peer_id: &str, socket_id: &str) -> Result<(), Box<dyn std::error::Error>> {
    let ice_servers = vec![
        RTCIceServer {
            urls: vec!["turn:a.relay.metered.ca:80".to_owned()],
            username: "f7d260b72ad1a7d7d2ad79c9".to_owned(),
            credential: "mEfyvohPVIda0MV2".to_owned(),
            ..Default::default()
        },
        RTCIceServer {
            urls: vec!["turn:a.relay.metered.ca:443".to_owned()],
            username: "f7d260b72ad1a7d7d2ad79c9".to_owned(),
            credential: "mEfyvohPVIda0MV2".to_owned(),
            ..Default::default()
        }
    ];

    let rtc_config = RTCConfiguration {
        ice_servers,
        ..Default::default()
    };

    // MediaEngine and API setup
    let mut media_engine = MediaEngine::default();
    media_engine.register_default_codecs()?;

    let mut registry = Registry::new();

    // Use the default set of Interceptors
    registry = register_default_interceptors(registry, &mut media_engine)?;

    let api = APIBuilder::new()
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .build();

    let pc = Arc::new(api.new_peer_connection(rtc_config).await?);
    let notify_tx = Arc::new(Notify::new());
    let (done_tx, mut done_rx) = tokio::sync::mpsc::channel::<()>(1);

    // Handle ice connection state changes
    let peer_connection = pc.clone();
    // Set the handler for ICE connection state
    // This will notify you when the peer has connected/disconnected
    peer_connection.on_ice_connection_state_change(Box::new(
        move |connection_state: RTCIceConnectionState| {
            println!("Connection State has changed {connection_state}");
            if connection_state == RTCIceConnectionState::Connected {
                notify_tx.notify_waiters();
            }
            Box::pin(async {})
        },
    ));

    // Set the handler for Peer connection state
    // This will notify you when the peer has connected/disconnected
    peer_connection.on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
        println!("Peer Connection State has changed: {s}");

        if s == RTCPeerConnectionState::Failed {
            // Wait until PeerConnection has had no network activity for 30 seconds or another failure. It may be reconnected using an ICE Restart.
            // Use webrtc.PeerConnectionStateDisconnected if you are interested in detecting faster timeout.
            // Note that the PeerConnection may come back from PeerConnectionStateDisconnected.
            println!("Peer Connection has gone to failed exiting");
            let _ = done_tx.try_send(());
        }

        Box::pin(async {})
    }));

    // Handle ice gathering state change
    pc.on_ice_gathering_state_change(Box::new(move |ice_gathering_state| {
        println!("ICE gathering state changed: {:?}", ice_gathering_state);
        Box::pin(async {})
    }));

    // Handle ice candidate
    pc.on_ice_candidate(Box::new(move |candidate: Option<RTCIceCandidate>| {
        if let Some(candidate) = candidate {
            println!("Sending ICE candidate");
            // Send the candidate through socket here
            // await sio.emit("deviceCandidate", {"candidate": candidate.to_string(), "to_socket_id": peer_id})
            socket.emit("deviceCandidate", json!({"candidate": candidate.to_string(), "to_socket_id": peer_id}));
        }
        Box::pin(async {})
    }));

    // Add a camera stream (this is a simple example; replace it with your camera stream implementation)
    let track = Arc::new(TrackLocalStaticSample::new(RTCRtpCodecCapability {
        mime_type: "video/vp8".to_string(),
        clock_rate: 90000,
        ..Default::default()
    }, "video".to_string(), "webrtc-rust".to_string()));

    pc.add_track(track.clone()).await?;

    // Create an offer
    let offer = pc.create_offer(None).await?;
    pc.set_local_description(offer).await?;

    // Send the offer to the peer (e.g., through Socket.IO)
    // await sio.emit("deviceOffer", {"offer": offer, "to_socket_id": peer_id})
    socket.emit("deviceOffer", json!({"offer": offer, "to_socket_id": peer_id}));

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging
    env_logger::init();

    // Create a Notify to wait for the 'connect' event
    let notify = Arc::new(Notify::new());
    let notify_clone = notify.clone();

    // Build the socket client
    let socket = ClientBuilder::new("https://signal.trl-ai.com/")
        .namespace("/") // Ensure the namespace matches your server configuration
        .on("connect", move |_, _| {
            async move {
                println!("Does this even work?");
            }.boxed()
        })
        .on("deviceConnect", move |_, _| {
            let notify = notify_clone.clone();
            async move {
                info!("Connected to the server.");
                // Notify that the connection is established
                notify.notify_one();
            }
            .boxed()
        })
        .on("deviceJoinRoom", |payload: Payload, _| {
            async move {
                match payload {
                    Payload::Text(text) => {
                        let room = String::new();
                        let id = String::new();
                        let peer_ids: Vec<String> = Vec::new();

                        if let Some(Value::Object(map)) = text.get(0) {
                            // Extract "room" from the JSON object
                            if let Some(Value::String(room)) = map.get("room") {
                                println!("Room: {}", room);
                                // Store or use `room` as needed
                            }
                
                            // Extract "id" from the JSON object
                            if let Some(Value::String(id)) = map.get("id") {
                                println!("ID: {}", id);
                                // Store or use `id` as needed
                            }
                
                            // Extract "peers" from the JSON object, which is expected to be an array of strings
                            if let Some(Value::Array(peers)) = map.get("peers") {
                                let peer_ids: Vec<String> = peers
                                    .iter()
                                    .filter_map(|p| p.as_str().map(String::from))
                                    .collect();
                
                                println!("Peer IDs: {:?}", peer_ids);
                                // Store or use `peer_ids` as needed
                            }

                            // Now that we have extracted `room`, `id`, and `peer_ids`, we can loop through `peer_ids` and set up WebRTC
                            if !room.is_empty() && !id.is_empty() && !peer_ids.is_empty() {
                                for peer_id in &peer_ids {
                                    if let Err(e) = setup_webrtc(&room, peer_id, &id).await {
                                        error!("Failed to set up WebRTC for peer {}: {:?}", peer_id, e);
                                    }
                                }
                            } else {
                                error!("Missing required information: room={}, id={}, peer_ids={:?}", room, id, peer_ids);
                            }
                        } else {
                            println!("Received unexpected JSON format: {:?}", text);
                        }
                    },
                    Payload::Binary(bin_data) => println!("Received binary data: {:#?}", bin_data),
                    Payload::String(data) => println!("Recieved String Data: {}", data)
                }
                
            }.boxed()
        })
        .on("connect_error", |err, _| {
            async move {
                error!("Connection error: {:?}", err);
            }
            .boxed()
        })
        .on("error", |err, _| {
            async move {
                error!("Error: {:?}", err);
            }
            .boxed()
        })
        .on_any(|event, payload, _| {
            async move {
                info!("Event: {}, Payload: {:?}", event, payload);
            }
            .boxed()
        })
        .connect()
        .await?;

    // Wait for the 'connect' event before emitting any custom events
    notify.notified().await;

    // Now it's safe to emit the 'deviceJoinRoom' event
    info!("Emitting deviceJoinRoom event.");
    socket
        .emit("deviceJoinRoom", json!({"room": "test234"}))
        .await
        .expect("Failed to emit deviceJoinRoom event");

    // Keep the client running to listen for events
    loop {
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}
