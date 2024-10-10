use futures_util::FutureExt;
use rust_socketio::{
    asynchronous::{Client, ClientBuilder},
    Payload,
};
use serde_json::json;
use serde_json::Value;
use std::time::Duration;
use log::info;
use env_logger;
use tokio::sync::{Mutex, Notify};
use std::sync::Arc; 
use webrtc::media::Sample;
use gstreamer as gst;
use gstreamer_app as gst_app;
use gstreamer::prelude::*;
use webrtc::{api::media_engine::MediaEngine, ice_transport::ice_credential_type::RTCIceCredentialType};
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::ice_transport::ice_connection_state::RTCIceConnectionState;
use webrtc::ice_transport::ice_gatherer_state::RTCIceGathererState;
use webrtc::ice_transport::ice_candidate::{RTCIceCandidate, RTCIceCandidateInit};
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use webrtc::track::track_local::TrackLocal;
use webrtc::api::APIBuilder;
use log::{error, warn};

const DEVICE_ID: &str = "123"; // Replace with your actual device ID
// Shared variable for camera stream (similar to Python example)
// struct CameraStream {
//     track: Arc<TrackLocalStaticSample>,
// }

async fn add_video_track(peer_connection: Arc<RTCPeerConnection>) {

    if !std::path::Path::new("/home/austinruth/webrtc-rpi/output_fixed264.mp4").exists() {
        error!("Video file does not exist at the given path");
        return;
    }    

    let codec_capability = RTCRtpCodecCapability {
        mime_type: "video/vp8".to_string(),
        clock_rate: 90000,
        ..Default::default()
    };

    // let track: Arc<dyn webrtc::track::track_local::TrackLocal + Send + Sync> =
    //     Arc::new(TrackLocalStaticSample::new(
    //         codec_capability,
    //         "video".to_string(),
    //         "webrtc-rust".to_string(),
    //     ));

    let video_track = Arc::new(TrackLocalStaticSample::new(
        codec_capability, 
        "video".to_string(), 
        "webrtc-rust".to_string()));

    let _ = peer_connection.add_track(Arc::clone(&video_track) as Arc<dyn TrackLocal + Send + Sync>)
    .await;

    // Start GStreamer to read from USB camera and push samples to WebRTC track
    tokio::spawn(async move {
        // Initialize GStreamer
        gst::init().expect("Failed to initialize GStreamer");

        let pipeline = gst::parse::launch(
            "v4l2src device=/dev/video0 ! video/x-raw,format=YUY2,width=640,height=480,framerate=30/1 ! videoconvert ! video/x-raw,format=I420 ! appsink name=sink",
        )
        .expect("Failed to parse pipeline description")
        .downcast::<gst::Pipeline>()
        .expect("Failed to downcast to Pipeline");

        // Retrieve the appsink element from the pipeline
        let appsink = pipeline
            .by_name("sink")
            .expect("Failed to get appsink from pipeline")
            .dynamic_cast::<gst_app::AppSink>()
            .expect("Failed to cast to AppSink");

        // Configure the appsink
        appsink.set_drop(false);
        appsink.set_max_buffers(1);

        // Set the pipeline to the Playing state
        match pipeline.set_state(gst::State::Playing) {
            Ok(gst::StateChangeSuccess::Success) => log::info!("GStreamer pipeline is now playing"),
            Ok(gst::StateChangeSuccess::Async) => log::info!("GStreamer pipeline is changing state asynchronously"),
            Err(err) => {
                error!("Failed to set pipeline to Playing state: {:?}", err);
                return;
            }
            _ => error!("Unexpected state change result"),
        }

        // Main loop to pull samples from appsink and send them via WebRTC
        loop {
            match appsink.pull_sample() {
                Ok(sample) => {
                    if let Some(buffer) = sample.buffer() {
                        if let Ok(map) = buffer.map_readable() {
                            let webrtc_sample = Sample {
                                data: map.as_slice().to_vec().into(),
                                duration: Duration::from_millis(33), // Approximately 30 FPS
                                ..Default::default()
                            };

                            if let Err(err) = video_track.write_sample(&webrtc_sample).await {
                                error!("Failed to write sample to track: {}", err);
                                break;
                            } else {
                                info!("Successfully wrote a sample to the track");
                            }
                        } else {
                            warn!("Failed to map buffer as readable");
                        }
                    } else {
                        warn!("Failed to get buffer from sample");
                    }
                }
                Err(err) => {
                    error!("Error pulling sample from appsink: {}", err);
                    break;
                }
            }
        }

        // Clean up the pipeline after the loop exits
        pipeline.set_state(gst::State::Null).expect("Unable to set the pipeline to the Null state");
        log::info!("GStreamer pipeline has been cleaned up");
    });
}

async fn create_peer_connection() -> RTCPeerConnection {
    let mut media_engine = MediaEngine::default();
    let _ = media_engine.register_default_codecs();

    let api = APIBuilder::new()
    .with_media_engine(media_engine)
    // .with_interceptor_registry(registry)
    .build();

    let ice_servers = vec![
        RTCIceServer {
            urls: vec!["turn:a.relay.metered.ca:80".to_owned()],
            username: "f7d260b72ad1a7d7d2ad79c9".to_owned(),
            credential: "mEfyvohPVIda0MV2".to_owned(),
            credential_type: RTCIceCredentialType::Password,
            ..Default::default()
        },
        RTCIceServer {
            urls: vec!["turn:a.relay.metered.ca:443".to_owned()],
            username: "f7d260b72ad1a7d7d2ad79c9".to_owned(),
            credential: "mEfyvohPVIda0MV2".to_owned(),
            credential_type: RTCIceCredentialType::Password,
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

    // async fn send_offer(socket: Client, pc: Arc<RTCPeerConnection>, room: &str, socket_id: &str, to_socket_id: &str) -> Result<(), Box<dyn std::error::Error>> {
    //     println!("Sending Offer to Peer...");
    //     let offer = pc.create_offer(None).await?;
    //     pc.set_local_description(offer).await?;
    //     socket.emit("deviceOffer", json!({
    //         "socketID": socket_id,
    //         "peerID": to_socket_id,
    //         "sdp": pc.local_description().await.as_ref().map(|desc| desc.sdp.to_string()).unwrap_or_default(),
    //         "type": pc.local_description().await.as_ref().map(|desc| desc.sdp_type.to_string()).unwrap_or_default(),
    //         "device_id": DEVICE_ID, // Replace with your actual device ID
    //         "room": room,
    //     })).await?;

    //     Ok(())
    // }

    let device_connect_callback = {
        let notify = notify.clone();
        move |_, socket: Client| {
            let notify = notify.clone();
            async move {
                info!("Connected to the server.");
                socket.emit("statusUpdate", json!({"currentStatus": "ready", "id": DEVICE_ID})).await.unwrap();
                // Notify that the connection has been established
                notify.notify_one();
            }.boxed()
        }
    };

    let connect_callback = |_, _socket: Client| {
        async move {
            println!("Connected to the server.");
        }.boxed()
    };

    let djr_callback = {
        let peer_connection_clone = Arc::clone(&peer_connection);  // Clone once outside the closure
        move |payload, socket: Client| {
            let peer_connection_clone = Arc::clone(&peer_connection_clone);  // Clone again inside the closure
            async move {
                match payload {
                    Payload::Binary(bin_data) => println!("Received binary data: {:#?}", bin_data),
                    #[allow(deprecated)]
                    Payload::String(data) => println!("Received String Data: {}", data),
                    Payload::Text(text) => {
                        let mut room = String::new();
                        let mut id = String::new();
                        let mut peer_ids: Vec<String> = Vec::new();
    
                        if let Some(Value::Object(map)) = text.get(0) {
                            // Extract "room" from the JSON object
                            if let Some(Value::String(room_value)) = map.get("room") {
                                println!("Room: {}", room_value);
                                room = room_value.clone();
                            }
    
                            // Extract "id" from the JSON object
                            if let Some(Value::String(id_value)) = map.get("id") {
                                println!("ID: {}", id_value);
                                id = id_value.clone();
                            }
    
                            // Extract "peers" from the JSON object
                            if let Some(Value::Array(peers)) = map.get("peers") {
                                peer_ids = peers
                                    .iter()
                                    .filter_map(|p| p.as_str().map(String::from))
                                    .collect();
                                println!("Peer IDs: {:?}", peer_ids);
                            }
    
                            // Now that we have extracted `room`, `id`, and `peer_ids`, we can loop through `peer_ids` and set up WebRTC
                            if !room.is_empty() && !id.is_empty() && !peer_ids.is_empty() {
                                for peer_id in &peer_ids {
                                    let pc = Arc::clone(&peer_connection_clone);  // Clone inside the loop
                                    println!("Sending Offer to Peer...");
                                    let offer = pc.create_offer(None).await.unwrap();
                                    pc.set_local_description(offer).await.unwrap();
                                    socket.emit("deviceOffer", json!({
                                        "socketID": id,
                                        "peerID": peer_id,
                                        "sdp": pc.local_description().await.as_ref().map(|desc| desc.sdp.to_string()).unwrap_or_default(),
                                        "type": pc.local_description().await.as_ref().map(|desc| desc.sdp_type.to_string()).unwrap_or_default(),
                                        "device_id": DEVICE_ID, // Replace with your actual device ID
                                        "room": room,
                                    })).await.unwrap();
                                }
                            }
                        }
                    }
                }
            }
            .boxed()
        }
    };
    

    let dc_callback = {
        let peer_connection_clone = Arc::clone(&peer_connection);  // Clone outside the closure
        move |payload, _socket| {
            let peer_connection_clone = Arc::clone(&peer_connection_clone);  // Clone again inside the async block
            async move {
                match payload {
                    Payload::Text(ice_candidate) => {
                        println!("ice_candidate {:?}", ice_candidate);
                        if let Some(first_value) = ice_candidate.first() {
                            println!("first_value {:?}", first_value);
                            match serde_json::from_value::<RTCIceCandidateInit>(first_value.clone()) {
                                Ok(candidate) => {
                                    if let Err(err) = peer_connection_clone.add_ice_candidate(candidate).await {
                                        error!("Failed to add ICE candidate: {:?}", err);
                                    }
                                }
                                Err(err) => {
                                    error!("Failed to deserialize ICE candidate: {:?}", err);
                                }
                            }
                        } else {
                            warn!("Received empty ICE candidate payload");
                        }
                    },
                    #[allow(deprecated)]
                    Payload::String(ice_candidate) => {
                        let candidate: RTCIceCandidateInit = serde_json::from_str(&ice_candidate).unwrap();
                        peer_connection_clone.add_ice_candidate(candidate).await.unwrap();
                    },
                    Payload::Binary(bin_data) => println!("Received binary data: {:#?}", bin_data),
                }
            }.boxed()
        }
    };

    let da_callback = {
        let peer_connection_clone = Arc::clone(&peer_connection);  // Clone outside the closure
        move |payload, _socket| {
            let peer_connection_clone = Arc::clone(&peer_connection_clone);  // Clone again inside the async block
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
                            peer_connection_clone.set_remote_description(answer).await.unwrap();
                        }
                    }
                }
            }.boxed()
        }
    };

    let disconnect_callback = |_, _socket: Client| {
        async move {
            println!("Disconnected from the server.");
        }.boxed()
    };

    let socket = Arc::new(Mutex::new(ClientBuilder::new("https://signal.trl-ai.com/")
        .namespace("/") // Ensure the namespace matches your server configuration
        .on("connect", connect_callback)
        .on("deviceConnect", device_connect_callback)
        .on("deviceJoinRoom", djr_callback)
        .on("deviceCandidate", dc_callback)
        .on("deviceAnswer", da_callback)
        .on("disconnect", disconnect_callback)
        .on_any(|event, payload, _| {
            async move {
                info!("Event: {}, Payload: {:?}", event, payload);
            }
            .boxed()
        })
        .connect()
        .await
        .expect("Connection failed")));

    peer_connection.on_ice_candidate(Box::new({
        let socket_clone = Arc::clone(&socket); // Clone the Arc
        move |candidate: Option<RTCIceCandidate>| {
            let socket_clone = Arc::clone(&socket_clone); // Clone again inside closure
            async move {
                if let Some(candidate) = candidate {
                    println!("Sending ICE candidate");
                    let socket = socket_clone.lock(); // Lock the Mutex to access the socket
                    socket.await.emit("deviceCandidate", json!({"room": "test234", "candidate": candidate.to_string(), "to_socket_id": "test"})).await.unwrap();
                }
            }.boxed()
        }
    }));

    peer_connection.on_ice_gathering_state_change(Box::new(move |state: RTCIceGathererState| {
        println!("ICE gathering state changed: {:?}", state);
        Box::pin(async {})
    }));
    
    peer_connection.on_ice_connection_state_change(Box::new(move |state: RTCIceConnectionState| {
        println!("ICE connection state changed: {:?}", state);
        Box::pin(async {})
    }));

    notify.notified().await;

    // Now it's safe to emit the 'deviceJoinRoom' event
    info!("Emitting deviceJoinRoom event.");
    socket.lock()
        .await.emit("deviceJoinRoom", json!({"room": "test234"}))
        .await
        .expect("Failed to emit deviceJoinRoom event");

    add_video_track(Arc::clone(&peer_connection)).await;
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging
    env_logger::init();

    // Create a Notify to wait for the 'connect' event
    let notify = Arc::new(Notify::new());

    // Create a new RTCPeerConnection
    let pc = Arc::new(create_peer_connection().await);

    setup_socket_io(pc.clone(), notify.clone()).await;

    // Keep the client running to listen for events
    loop {
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}
