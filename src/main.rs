use futures_util::SinkExt;
use futures_util::StreamExt;
use serde_json::json;
use serde_json::Value;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::handshake::client::Request;
use tokio_tungstenite::tungstenite::http;
use tokio_tungstenite::WebSocketStream;
use std::time::Duration;
use log::{info, error, warn};
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
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use http::header::{HeaderValue, SEC_WEBSOCKET_PROTOCOL};

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

async fn add_video_track(peer_connection: Arc<RTCPeerConnection>) {
    info!("Adding video track to the peer connection");

    let codec_capability = RTCRtpCodecCapability {
        mime_type: "video/vp8".to_string(),
        clock_rate: 90000,
        ..Default::default()
    };

    let video_track = Arc::new(TrackLocalStaticSample::new(
        codec_capability,
        "video".to_string(),
        "webrtc-rust".to_string(),
    ));

    let result = peer_connection
        .add_track(Arc::clone(&video_track) as Arc<dyn TrackLocal + Send + Sync>)
        .await;

    if result.is_err() {
        error!("Failed to add video track to the peer connection: {:?}", result);
    } else {
        info!("Video track added successfully");
    }

    tokio::spawn(async move {
        info!("Starting GStreamer pipeline to capture video");
        gst::init().expect("Failed to initialize GStreamer");

        let pipeline = gst::parse::launch(
            "v4l2src device=/dev/video0 ! videoconvert ! vp8enc target-bitrate=3000000 deadline=1 cpu-used=2 threads=4 ! appsink name=sink",
        )
        .expect("Failed to parse GStreamer pipeline description")
        .downcast::<gst::Pipeline>()
        .expect("Failed to downcast to Pipeline");

        let appsink = pipeline
            .by_name("sink")
            .expect("Failed to get appsink from pipeline")
            .dynamic_cast::<gst_app::AppSink>()
            .expect("Failed to cast to AppSink");

        appsink.set_drop(false);
        appsink.set_max_buffers(1);

        info!("Setting GStreamer pipeline to PLAYING state");
        match pipeline.set_state(gst::State::Playing) {
            Ok(gst::StateChangeSuccess::Success) => info!("GStreamer pipeline is now playing"),
            Ok(gst::StateChangeSuccess::Async) => info!("GStreamer pipeline is changing state asynchronously"),
            Err(err) => {
                error!("Failed to set GStreamer pipeline to Playing state: {:?}", err);
                return;
            }
            _ => error!("Unexpected state change result"),
        }

        loop {
            match appsink.pull_sample() {
                Ok(sample) => {
                    if let Some(buffer) = sample.buffer() {
                        if let Ok(map) = buffer.map_readable() {
                            let webrtc_sample = Sample {
                                data: map.as_slice().to_vec().into(),
                                duration: Duration::from_millis(33),
                                ..Default::default()
                            };

                            if let Err(err) = video_track.write_sample(&webrtc_sample).await {
                                error!("Failed to write sample to video track: {}", err);
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

        info!("Stopping GStreamer pipeline");
        pipeline.set_state(gst::State::Null).expect("Failed to set the pipeline to NULL state");
    });
}

async fn create_peer_connection() -> RTCPeerConnection {
    info!("Creating a new WebRTC peer connection");

    let mut media_engine = MediaEngine::default();
    let mut setting_engine = SettingEngine::default();
    media_engine.register_default_codecs().unwrap();

    let network_types = vec![NetworkType::Udp4, NetworkType::Tcp4];
    setting_engine.set_network_types(network_types);

    let api = APIBuilder::new()
        .with_media_engine(media_engine)
        .with_setting_engine(setting_engine)
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
    ];

    let rtc_config = RTCConfiguration { ice_servers, ..Default::default() };

    let peer_connection = api.new_peer_connection(rtc_config).await;
    match peer_connection {
        Ok(pc) => {
            info!("Peer connection created successfully");
            pc
        }
        Err(err) => {
            error!("Failed to create peer connection: {:?}", err);
            panic!("Cannot proceed without a peer connection");
        }
    }
}

async fn get_room() -> Option<String> {
    info!("Fetching the current room for user {}", *USER_ID);

    let token = generate_token();
    let url = format!("https://scope-api.trl-ai.com/users/{}/current_room", *USER_ID);
    let client = ReqwestClient::new();

    let response = match client.get(&url).header("Authorization", format!("Bearer {}", token)).send().await {
        Ok(resp) => resp,
        Err(err) => {
            error!("Failed to send request to get room: {}", err);
            return None;
        }
    };

    if response.status() == StatusCode::OK {
        match response.json::<Value>().await {
            Ok(data) => match data.get("current_room") {
                Some(Value::String(room)) if !room.is_empty() => {
                    info!("User is in room: {}", room);
                    Some(room.clone())
                }
                _ => {
                    warn!("No valid room found for user");
                    None
                }
            },
            Err(err) => {
                error!("Failed to parse room data: {}", err);
                None
            }
        }
    } else {
        error!("Failed to fetch room. Status code: {}", response.status());
        None
    }
}

fn generate_token() -> String {
    info!("Generating JWT token for user {}", *USER_ID);
    let claims = Claims { user_id: (*USER_ID.to_owned()).to_string() };

    let header = Header { alg: Algorithm::HS256, ..Default::default() };
    let encoding_key = EncodingKey::from_secret(JWT_SECRET.as_bytes());

    match encode(&header, &claims, &encoding_key) {
        Ok(token) => {
            info!("Token generated successfully");
            token
        }
        Err(err) => {
            error!("Failed to generate token: {}", err);
            String::new()
        }
    }
}

#[derive(Debug, Serialize)]
struct Claims {
    user_id: String,
}

async fn wait_for_room() {
    loop {
        match get_room().await {
            Some(room) => {
                info!("Room found: {}", room);
                if let Ok(room_id) = room.parse::<u64>() {
                    if ROOM.set(room_id).is_ok() {
                        break;
                    } else {
                        warn!("Failed to set room ID");
                    }
                }
            }
            None => info!("No room found, retrying..."),
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

async fn create_janus_session(ws_stream: &mut WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>) -> Result<u64, Box<dyn std::error::Error>> {
    info!("Creating Janus session");

    let transaction_id = format!("create-session-{}", chrono::Utc::now().timestamp_millis());
    let session_message = json!({ "janus": "create", "transaction": transaction_id });

    ws_stream.send(Message::Text(session_message.to_string())).await?;

    if let Some(msg) = ws_stream.next().await {
        if let Ok(Message::Text(response)) = msg {
            let parsed: serde_json::Value = serde_json::from_str(&response)?;
            let session_id = parsed["data"]["id"].as_u64().unwrap();
            info!("Janus session created with ID: {}", session_id);
            return Ok(session_id);
        }
    }

    Err("Failed to create Janus session".into())
}

async fn attach_videoroom_plugin(
    ws_stream: &mut WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>,
    session_id: u64,
) -> Result<u64, Box<dyn std::error::Error>> {
    info!("Attaching to VideoRoom plugin");

    let transaction_id = format!("attach-plugin-{}", chrono::Utc::now().timestamp_millis());
    let attach_message = json!({
        "janus": "attach",
        "plugin": "janus.plugin.videoroom",
        "session_id": session_id,
        "transaction": transaction_id
    });

    ws_stream.send(Message::Text(attach_message.to_string())).await?;

    if let Some(msg) = ws_stream.next().await {
        if let Ok(Message::Text(response)) = msg {
            let parsed: serde_json::Value = serde_json::from_str(&response)?;
            let handle_id = parsed["data"]["id"].as_u64().unwrap();
            info!("Attached to VideoRoom plugin with handle ID: {}", handle_id);
            return Ok(handle_id);
        }
    }

    Err("Failed to attach to VideoRoom plugin".into())
}

async fn join_room(
    ws_stream: &mut WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>,
    session_id: u64,
    handle_id: u64,
    room_id: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    info!("Joining room with ID: {}", room_id);

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

    if let Some(msg) = ws_stream.next().await {
        if let Ok(Message::Text(response)) = msg {
            let parsed: serde_json::Value = serde_json::from_str(&response)?;
            if parsed["janus"] == "success" {
                info!("Successfully joined room with ID: {}", room_id);
                return Ok(());
            }
        }
    }

    Err("Failed to join room".into())
}

async fn send_webrtc_offer(
    ws_stream: &mut WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>,
    session_id: u64,
    handle_id: u64,
    offer_sdp: String,
) -> Result<String, Box<dyn std::error::Error>> {
    info!("Sending WebRTC SDP offer");

    let transaction_id = format!("send-offer-{}", chrono::Utc::now().timestamp_millis());
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

    while let Some(msg) = ws_stream.next().await {
        if let Ok(Message::Text(response)) = msg {
            let parsed: serde_json::Value = serde_json::from_str(&response)?;
            if parsed["janus"] == "success" {
                if let Some(jsep) = parsed["jsep"].as_object() {
                    if let Some(sdp) = jsep["sdp"].as_str() {
                        info!("SDP answer received from Janus");
                        return Ok(sdp.to_string());
                    }
                }
            }
        }
    }

    Err("Failed to send WebRTC offer and receive SDP answer".into())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();
    info!("Starting Janus WebRTC client");

    let url = "wss://janus.trl-ai.com/janus";

    // Create a WebSocket request and add the Sec-WebSocket-Protocol header
    let mut request = url.into_client_request()?;
    request.headers_mut().insert(
        SEC_WEBSOCKET_PROTOCOL,
        HeaderValue::from_str("janus-protocol")?,
    );

    // Establish the WebSocket connection using the modified request
    let (mut ws_stream, _) = connect_async(request).await?;
    println!("WebSocket connection established");

    wait_for_room().await;

    let session_id = create_janus_session(&mut ws_stream).await?;
    info!("Janus session created with ID: {}", session_id);

    let handle_id = attach_videoroom_plugin(&mut ws_stream, session_id).await?;
    info!("Attached to VideoRoom plugin with handle ID: {}", handle_id);

    let room_id = *ROOM.get().expect("ROOM should be set");
    join_room(&mut ws_stream, session_id, handle_id, room_id).await?;
    info!("Joined the room with ID: {}", room_id);

    let pc = Arc::new(create_peer_connection().await);
    add_video_track(Arc::clone(&pc)).await;

    let offer = pc.create_offer(None).await.unwrap();
    pc.set_local_description(offer.clone()).await.unwrap();
    info!("Local SDP offer created and set");

    let sdp_answer = send_webrtc_offer(&mut ws_stream, session_id, handle_id, offer.sdp).await?;
    pc.set_remote_description(RTCSessionDescription::answer(sdp_answer)?).await.unwrap();
    info!("Remote SDP answer set");

    loop {
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}
