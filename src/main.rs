use futures_util::SinkExt;
use futures_util::StreamExt;
use serde_json::json;
use serde_json::Value;
use tokio::net::TcpStream;
use tokio_tungstenite::WebSocketStream;
use std::time::Duration;
use log::info;
use env_logger;
use std::sync::Arc; 
use webrtc::media::Sample;
use gstreamer as gst;
use gstreamer_app as gst_app;
use gstreamer::prelude::*;
use webrtc::{api::media_engine::MediaEngine, ice_transport::ice_credential_type::RTCIceCredentialType};
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
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
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::protocol::Message;


static JWT_SECRET: Lazy<String> = Lazy::new(|| {
    dotenv().ok();
    env::var("JWT_SECRET").expect("JWT_SECRET must be set")
});

static USER_ID: Lazy<String> = Lazy::new(|| {
    dotenv().ok();
    env::var("USER_ID").expect("USER_ID must be set")
});

#[allow(dead_code)]
static DEVICE_ID: Lazy<String> = Lazy::new(|| {
    dotenv().ok();
    env::var("DEVICE_ID").expect("DEVICE_ID must be set")
});

static ROOM: OnceCell<u64> = OnceCell::const_new();

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
                if let Ok(room_id) = room.parse::<u64>() {
                    if ROOM.set(room_id).is_ok() {
                        break;
                    } else {
                        warn!("Failed to set the room.");
                    }
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

// Update `create_janus_session` to accept ws_stream
async fn create_janus_session(ws_stream: &mut WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>) -> Result<u64, Box<dyn std::error::Error>> {
    let transaction_id = format!("create-session-{}", chrono::Utc::now().timestamp_millis());

    // Send the session creation request
    let session_message = json!({
        "janus": "create",
        "transaction": transaction_id
    });
    ws_stream.send(Message::Text(session_message.to_string())).await?;

    // Wait for the response
    if let Some(msg) = ws_stream.next().await {
        if let Ok(Message::Text(response)) = msg {
            let parsed: serde_json::Value = serde_json::from_str(&response)?;
            let session_id = parsed["data"]["id"].as_u64().unwrap();
            return Ok(session_id);
        }
    }

    Err("Failed to create session".into())
}

async fn attach_videoroom_plugin(ws_stream: &mut WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>, session_id: u64) -> Result<u64, Box<dyn std::error::Error>> {
    let transaction_id = format!("attach-plugin-{}", chrono::Utc::now().timestamp_millis());

    // Attach to the plugin
    let attach_message = json!({
        "janus": "attach",
        "plugin": "janus.plugin.videoroom",
        "session_id": session_id,
        "transaction": transaction_id
    });
    
    ws_stream.send(Message::Text(attach_message.to_string())).await?;

    // Wait for the plugin attachment response
    if let Some(msg) = ws_stream.next().await {
        if let Ok(Message::Text(response)) = msg {
            let parsed: serde_json::Value = serde_json::from_str(&response)?;
            let handle_id = parsed["data"]["id"].as_u64().unwrap();
            return Ok(handle_id);
        }
    }

    Err("Failed to attach to videoroom plugin".into())
}

async fn join_room(ws_stream: &mut WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>, session_id: u64, handle_id: u64, room_id: u64) -> Result<(), Box<dyn std::error::Error>> {
    let transaction_id = format!("join-room-{}", chrono::Utc::now().timestamp_millis());

    let join_message = json!({
        "janus": "message",
        "body": {
            "request": "join",
            "ptype": "publisher",
            "room": room_id,
        },
        "session_id": session_id,
        "handle_id": handle_id,
        "transaction": transaction_id
    });
    
    ws_stream.send(Message::Text(join_message.to_string())).await?;

    // Handle the response
    if let Some(msg) = ws_stream.next().await {
        if let Ok(Message::Text(response)) = msg {
            let parsed: serde_json::Value = serde_json::from_str(&response)?;
            if parsed["janus"] == "success" {
                println!("Joined the room successfully");
                return Ok(());
            }
        }
    }

    Err("Failed to join the room".into())
}

async fn send_webrtc_offer(
    ws_stream: &mut WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>, 
    session_id: u64, 
    handle_id: u64, 
    offer_sdp: String
) -> Result<String, Box<dyn std::error::Error>> {
    let transaction_id = format!("send-offer-{}", chrono::Utc::now().timestamp_millis());

    // Send the SDP offer to Janus
    let offer_message = json!({
        "janus": "message",
        "body": {
            "request": "publish",
            "audio": true,
            "video": true,
            "sdp": offer_sdp,
        },
        "jsep": {
            "type": "offer",
            "sdp": offer_sdp
        },
        "transaction": transaction_id,
        "session_id": session_id,
        "handle_id": handle_id
    });

    ws_stream.send(Message::Text(offer_message.to_string())).await?;

    // Handle the response and extract the SDP answer
    while let Some(msg) = ws_stream.next().await {
        if let Ok(Message::Text(response)) = msg {
            let parsed: serde_json::Value = serde_json::from_str(&response)?;
            if parsed["janus"] == "success" {
                if let Some(jsep) = parsed["jsep"].as_object() {
                    if let Some(sdp) = jsep["sdp"].as_str() {
                        return Ok(sdp.to_string()); // Return the SDP answer
                    }
                }
            }
        }
    }

    Err("Failed to send WebRTC offer and receive SDP answer".into())
}


#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging
    env_logger::init();

    // 1. Establish WebSocket connection
    let (mut ws_stream, _) = connect_async("wss://janus.trl-ai.com/janus").await?;
    
    // 2. Wait for a valid room to be set
    wait_for_room().await;

    // 3. Create a Janus session
    let session_id = create_janus_session(&mut ws_stream).await?;

    // 4. Attach to the videoroom plugin and get handle_id
    let handle_id = attach_videoroom_plugin(&mut ws_stream, session_id).await?;

    let room_id = *ROOM.get().expect("ROOM should be set");
    join_room(&mut ws_stream, session_id, handle_id, room_id).await?;

    // 5. Create a new WebRTC peer connection
    let pc = Arc::new(create_peer_connection().await);

    // 6. Add video track to the peer connection
    add_video_track(Arc::clone(&pc)).await;

    // 7. Create an SDP offer
    let offer = pc.create_offer(None).await.unwrap();
    pc.set_local_description(offer.clone()).await.unwrap();

    // 8. Send the offer to Janus and receive the SDP answer
    let sdp_answer = send_webrtc_offer(&mut ws_stream, session_id, handle_id, offer.sdp).await?;

    // 9. Set the SDP answer as the remote description
    pc.set_remote_description(RTCSessionDescription::answer(sdp_answer)?).await.unwrap();

    // Keep the client running to listen for events
    loop {
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}