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
use webrtc::{api::setting_engine, media::Sample};
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
use reqwest::StatusCode;
use reqwest::Client as ReqwestClient;
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde::Serialize;
use dotenv::dotenv;
use std::env;
use once_cell::sync::Lazy;
use tokio::sync::OnceCell;
use webrtc::api::setting_engine::SettingEngine;
use webrtc::ice::network_type::NetworkType;


static JWT_SECRET: Lazy<String> = Lazy::new(|| {
    dotenv().ok();
    env::var("JWT_SECRET").expect("JWT_SECRET must be set")
});

static USER_ID: Lazy<String> = Lazy::new(|| {
    dotenv().ok();
    env::var("USER_ID").expect("USER_ID must be set")
});

static DEVICE_ID: Lazy<String> = Lazy::new(|| {
    dotenv().ok();
    env::var("DEVICE_ID").expect("DEVICE_ID must be set")
});

static ROOM: OnceCell<String> = OnceCell::const_new();

// Shared variable for camera stream (similar to Python example)
// struct CameraStream {
//     track: Arc<TrackLocalStaticSample>,
// }

async fn add_video_track(peer_connection: Arc<RTCPeerConnection>) {

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
            "v4l2src device=/dev/video0 ! videoconvert ! vp8enc target-bitrate=3000000 deadline=1 cpu-used=2 threads=4 ! appsink name=sink"
            // "avfvideosrc ! videoconvert ! vp8enc ! appsink name=sink",
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
    let mut setting_engine = SettingEngine::default();
    media_engine.register_default_codecs().unwrap();

    // Restrict network types to IPv4
    let network_types = vec![
        NetworkType::Udp4,
        NetworkType::Tcp4,
    ];

    setting_engine.set_network_types(network_types.clone());

    let api = APIBuilder::new()
    .with_media_engine(media_engine)
    .with_setting_engine(setting_engine)
    // .with_interceptor_registry(registry)
    .build();

    let ice_servers = vec![
        RTCIceServer {
            urls: vec!["stun:stun.relay.metered.ca:80".to_owned()],
            ..Default::default()
        },
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
        },
        RTCIceServer {
            urls: vec!["turn:a.relay.metered.ca:80?transport=tcp".to_owned()],
            username: "f7d260b72ad1a7d7d2ad79c9".to_owned(),
            credential: "mEfyvohPVIda0MV2".to_owned(),
            credential_type: RTCIceCredentialType::Password,
            ..Default::default()
        },
        RTCIceServer {
            urls: vec!["turn:a.relay.metered.ca:443?transport=tcp".to_owned()],
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

    let device_connect_callback = {
        let notify = notify.clone();
        move |_, socket: Client| {
            let notify = notify.clone();
            async move {
                info!("Connected to the server.");
                socket.emit("statusUpdate", json!({"currentStatus": "ready", "id": *DEVICE_ID})).await.unwrap();
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
                                    println!("Sending Offer to Peer... {:?}", peer_id);
                                    let offer = pc.create_offer(None).await.unwrap();
                                    pc.set_local_description(offer).await.unwrap();
                                    socket.emit("deviceOffer", json!({
                                        "socketID": id,
                                        "peerID": peer_id,
                                        "sdp": pc.local_description().await.as_ref().map(|desc| desc.sdp.to_string()).unwrap_or_default(),
                                        "type": pc.local_description().await.as_ref().map(|desc| desc.sdp_type.to_string()).unwrap_or_default(),
                                        "device_id": *DEVICE_ID, // Replace with your actual device ID
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

    fn extract_ice_candidate(payload: &Vec<Value>) -> Result<(serde_json::Value, String), &'static str> {
        let mut candidate_json = Value::Null;
        let mut additional_string = String::new();
    
        // Extracting the ICE candidate object and additional string from the payload
        if let Some(object) = payload.get(0) {
            if let Value::Object(candidate_object) = object {
                // Create a new JSON object containing only the needed fields
                candidate_json = json!({
                    "candidate": candidate_object.get("candidate"),
                    "sdpMLineIndex": candidate_object.get("sdpMLineIndex"),
                    "sdpMid": candidate_object.get("sdpMid"),
                    "usernameFragment": candidate_object.get("usernameFragment")
                });
            }
        }
    
        // Extracting the additional string from the payload
        if let Some(Value::String(additional_str)) = payload.get(1) {
            additional_string = additional_str.clone();
        }
    
        Ok((candidate_json, additional_string))
    }
    

    let dc_callback = {
        let peer_connection_clone = Arc::clone(&peer_connection);  // Clone outside the closure
        move |payload, _socket| {
            let peer_connection_clone = Arc::clone(&peer_connection_clone);  // Clone again inside the async block
            async move {
                match payload {
                    Payload::Text(ice_candidate) => {
                        // Payload - ice_candidate should have the following format:
                        // {
                        //     {"candidate": String("candidate:3139602211 1 udp 2122260223 192.168.4.29 54188 typ host generation 0 ufrag 5p7r network-id 1 network-cost 10"), "sdpMLineIndex": Number(0), "sdpMid": String("0"), "usernameFragment": Null}, 
                        //     String
                        // }

                        // Extract the ICE candidate object and additional string from the payload
                        match extract_ice_candidate(&ice_candidate) {
                            Ok((candidate_json, _additional_string)) => {
                                let candidate: RTCIceCandidateInit = serde_json::from_value(candidate_json).unwrap();
                                // Use the candidate and additional_string as needed
                                // println!("ICE Candidate: {:#?}", candidate);
                                // println!("Additional String: {}", additional_string);
                                let _ = peer_connection_clone.add_ice_candidate(candidate).await;
                            },
                            Err(e) => {
                                println!("Error extracting ICE candidate: {}", e);
                                return;
                            }
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
                println!("Device Answer Callback Triggered...");
                println!{"Payload {:?}", payload}
                match payload {
                    Payload::Text(text) => {

                        if text.len() != 3 {
                            error!("Payload does not contain exactly three elements");
                        }
                    
                        let peer_id = match &text[0] {
                            Value::String(s) => s.clone(),
                            _ => {
                                error!("First payload element is not a String");
                                return;
                            }
                        };
                    
                        let answer = match &text[1] {
                            Value::Object(obj) => obj.clone(),
                            _ => {
                                error!("Second payload element is not an Object");
                                return;
                            }
                        };
                    
                        let device_id = match &text[2] {
                            Value::String(s) => s.clone(),
                            _ => {
                                error!("Third payload element is not a String");
                                return;
                            }
                        };
                    
                        info!("Parsed payload successfully");
        
                        println!("Peer ID: {}", peer_id);
                        println!("Answer: {:?}", answer);
                        println!("Device ID: {}", device_id);
        
                        if device_id == *DEVICE_ID {
                            let answer_json = serde_json::to_string(&answer).unwrap();
                            let answer: RTCSessionDescription = serde_json::from_str(&answer_json).unwrap();
                            peer_connection_clone.set_remote_description(answer).await.unwrap();
                        }
                    },
                    Payload::Binary(bin_data) => println!("Received binary data: {:#?}", bin_data),
                    #[allow(deprecated)]
                    Payload::String(data) => println!("Received String data: {:?}", data)
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
                    // println!("Candidate: {:?}", candidate);
                    let socket = socket_clone.lock(); // Lock the Mutex to access the socket
                    let room = ROOM.get().expect("ROOM should be set");
                    socket.await.emit("deviceCandidate", json!({"room": room, "candidate": candidate.to_string(), "peerID": "test"})).await.unwrap();
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
    let room = ROOM.get().expect("ROOM should be set");
    socket.lock()
        .await.emit("deviceJoinRoom", json!({"room": room}))
        .await
        .expect("Failed to emit deviceJoinRoom event");

    add_video_track(Arc::clone(&peer_connection)).await;
}

async fn get_room() -> Option<String> {
    info!("Checking for User in valid room...");
    let token = generate_token(); // Assume this function is defined

    let url = format!("https://scope-api.trl-ai.com/users/{}/current_room", *USER_ID);

    let client = ReqwestClient::new();

    let response = match client
        .get(&url)
        .header("Authorization", format!("Bearer {}", token))
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(err) => {
            error!("Failed to send request: {}", err);
            return None;
        }
    };

    if response.status() == StatusCode::OK {
        let data: Value = match response.json().await {
            Ok(json) => json,
            Err(err) => {
                error!("Failed to parse response JSON: {}", err);
                return None;
            }
        };

        match data.get("current_room") {
            Some(Value::String(s)) if s.is_empty() => {
                error!("User hasn't joined a room...");
                None
            }
            Some(Value::String(s)) => Some(s.clone()),
            _ => {
                error!("'current_room' field is missing or not a string");
                None
            }
        }
    } else {
        error!("Failed to fetch current room: {}", response.status());
        None
    }
}

#[derive(Debug, Serialize)]
struct Claims {
    user_id: String,
    // You can add other fields as needed
}

fn generate_token() -> String {
    info!("Generating Token...");
    let claims = Claims {
        user_id: (*USER_ID.to_owned()).to_string(),
    };

    let header = Header {
        alg: Algorithm::HS256,
        ..Default::default()
    };

    let encoding_key = EncodingKey::from_secret(JWT_SECRET.as_bytes());

    match encode(&header, &claims, &encoding_key) {
        Ok(token) => token,
        Err(err) => {
            log::error!("Failed to generate token: {}", err);
            String::new()
        }
    }
}

async fn wait_for_room() {
    loop {
        match get_room().await {
            Some(room) => {
                info!("Found a valid room: {}", room);
                if ROOM.set(room.clone()).is_ok() {
                    break;
                } else {
                    warn!("Failed to set the room.");
                }
            }
            None => {
                info!("No valid room found, retrying...");
            }
        }
        // Sleep before trying again
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}


#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging
    env_logger::init();

    wait_for_room().await;

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
