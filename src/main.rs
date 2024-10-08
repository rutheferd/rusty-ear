use futures_util::FutureExt;
use log::logger;
use rust_socketio::{
    Client,
    asynchronous::ClientBuilder, Payload
};
use serde_json::json;
use serde_json::Value;
use webrtc::ice::candidate;
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

const DEVICE_ID: &str = "123"; // Replace with your actual device ID
// Shared variable for camera stream (similar to Python example)
struct CameraStream {
    track: Arc<TrackLocalStaticSample>,
}

async fn send_offer(socket: Arc<Client>, pc: Arc<RTCPeerConnection>, room: &str, socket_id: &str, to_socket_id: &str) -> Result<(), Box<dyn std::error::Error>> {
    logger().info("Sending offer to peer");
    let offer = pc.create_offer(None).await?;
    pc.set_local_description(offer).await?;
    socket.emit("deviceOffer", json!({
        "socketID": socket_id,
        "peerID": to_socket_id,
        "sdp": pc.local_description().sdp,
        "type": pc.local_description()?.as_ref().map(|desc| desc.sdp_type.to_string()).unwrap_or_default(),
        "device_id": DEVICE_ID, // Replace with your actual device ID
        "room": room,
    })).await?;

    Ok(())
}

async fn create_peer_connection() -> RTCPeerConnection {
    let mut media_engine = MediaEngine::default();
    media_engine.register_default_codecs()?;

    let api = APIBuilder::new()
    .with_media_engine(media_engine)
    .with_interceptor_registry(registry)
    .build();

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

    api.new_peer_connection(rtc_config).await.unwrap()
}

async fn setup_socket_io(peer_connection: Arc<RTCPeerConnection>, notify: Arc<Notify>) {
    let socket = ClientBuilder::new("https://signal.trl-ai.com/")
        .namespace("/") // Ensure the namespace matches your server configuration
        .on("connect", move |_, _| {
            async move {
                println!("Does this even work?");
                socket.emit("statusUpdate", json!({"currentStatus": "ready", "id": DEVICE_ID}));
            }.boxed()
        })
        .on("deviceConnect", move |_, _| {
            let notify = notify.clone();
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
                                    let peer_connection = peer_connection.clone();
                                    send_offer(socket, pc, &room, socket_id, to_socket_id)
                                        .await
                                        .expect("Failed to send offer");
                                }
                            }
                        }
                    },
                    Payload::Binary(bin_data) => println!("Received binary data: {:#?}", bin_data),
                    Payload::String(data) => println!("Recieved String Data: {}", data)
                }
            }.boxed()
        })
        .on("deviceCandidate", |candidate, _| {
            let peer_connection = peer_connection.clone();
            async move {
                if let Payload::String(ice_candidate) = payload {
                    let candidate: RTCIceCandidate = serde_json::from_str(&ice_candidate).unwrap();
                    peer_connection.add_ice_candidate(candidate).await.unwrap();
                }
            }.boxed()
        })
        .on("deviceAnswer", |payload, _| {
            let peer_connection = peer_connection.clone();
            // extract peer_id, answer, and device_id from the payload
            async move {
                if let Payload::Text(text) = payload {
                    if let Some(Value::Object(map)) = text.get(0) {
                        let peer_id = map.get("peer_id").and_then(Value::as_str).unwrap_or_default().to_string();
                        let answer = map.get("answer").and_then(Value::as_str).unwrap_or_default().to_string();
                        let device_id = map.get("device_id").and_then(Value::as_str).unwrap_or_default().to_string();
    
                        println!("Peer ID: {}", peer_id);
                        println!("Answer: {}", answer);
                        println!("Device ID: {}", device_id);
    
                        if device_id == DEVICE_ID {
                            let answer: RTCSessionDescription = serde_json::from_str(&answer).unwrap();
                            peer_connection.set_remote_description(answer).await.unwrap();
                        }
                    }
                }
            }.boxed()
        })
        .on("disconnect", |_, _| {
            async move {
                println!("Disconnected from the server.");
            }.boxed()
        })
        .connect()
        .await?;

    peer_connection.on_ice_candidate(Box::new(move |candidate: Option<RTCIceCandidate>| {
        if let Some(candidate) = candidate {
            println!("Sending ICE candidate");
            socket.emit("deviceCandidate", json!({"candidate": candidate.to_string(), "to_socket_id": peer_id}));
        }
        Box::pin(async {})
    }));

    peer_connection.on_ice_gathering_state_change(future::poll_fn(|cx| {
        println!("ICE gathering state changed: {:?}", peer_connection.ice_gathering_state());
        future::ready(())
    }));

    peer_connection.on_ice_connection_state_change(future::poll_fn(|cx| {
        println!("ICE connection state changed: {:?}", peer_connection.ice_connection_state());
        future::ready(())
    }));
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging
    env_logger::init();

    // Create a Notify to wait for the 'connect' event
    let notify = Arc::new(Notify::new());

    // Create a new RTCPeerConnection
    let pc = Arc::new(create_peer_connection().await);

    setup_socket_io(peer_connection.clone(), notify.clone()).await;

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
