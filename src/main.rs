use futures_util::stream::SplitSink;
use futures_util::stream::SplitStream;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use serde_json::Value;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::handshake::client::Request;
use tokio_tungstenite::tungstenite::http;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::WebSocketStream;
use webrtc::ice_transport::ice_candidate::RTCIceCandidate;
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
use std::time::Duration;
use std::time::SystemTime;
use log::{info, error, warn, debug};
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
use tokio::sync::Mutex;

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
            // "v4l2src device=/dev/video0 ! videoconvert ! vp8enc target-bitrate=3000000 deadline=1 cpu-used=2 threads=4 ! appsink name=sink",
            "avfvideosrc ! videoconvert ! vp8enc target-bitrate=3000000 deadline=1 cpu-used=2 threads=4 ! appsink name=sink",
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
                            let pts_duration = buffer.pts().map(|t| Duration::from_nanos(t.nseconds())).unwrap_or(Duration::from_nanos(0));
                            let webrtc_sample = Sample {
                                data: map.as_slice().to_vec().into(),
                                duration: Duration::from_millis(33),
                                timestamp: SystemTime::now() - pts_duration,
                                ..Default::default()
                            };

                            if let Err(err) = video_track.write_sample(&webrtc_sample).await {
                                error!("Failed to write sample to video track: {}", err);
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
            urls: vec!["stun:stun.l.google.com:19302".to_owned()],
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

async fn create_janus_session(
    ws_sender: &mut SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
    ws_receiver: &mut SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
) -> Result<u64, Box<dyn std::error::Error>> {
    info!("Creating Janus session");

    let transaction_id = format!("create-session-{}", chrono::Utc::now().timestamp_millis());
    let session_message = json!({ "janus": "create", "transaction": transaction_id });

    ws_sender.send(Message::Text(session_message.to_string())).await?;

    while let Some(msg) = ws_receiver.next().await {
        if let Ok(Message::Text(response)) = msg {
            let parsed: serde_json::Value = serde_json::from_str(&response)?;
            if parsed["transaction"] == transaction_id {
                let session_id = parsed["data"]["id"].as_u64().unwrap();
                info!("Janus session created with ID: {}", session_id);
                return Ok(session_id);
            }
        }
    }

    Err("Failed to create Janus session".into())
}

async fn attach_videoroom_plugin(
    ws_sender: &mut SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
    ws_receiver: &mut SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
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

    ws_sender.send(Message::Text(attach_message.to_string())).await?;

    while let Some(msg) = ws_receiver.next().await {
        if let Ok(Message::Text(response)) = msg {
            let parsed: serde_json::Value = serde_json::from_str(&response)?;
            if parsed["transaction"] == transaction_id {
                let handle_id = parsed["data"]["id"].as_u64().unwrap();
                info!("Attached to VideoRoom plugin with handle ID: {}", handle_id);
                return Ok(handle_id);
            }
        }
    }

    Err("Failed to attach to VideoRoom plugin".into())
}

async fn join_room(
    ws_sender: &mut SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
    ws_receiver: &mut SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    session_id: u64,
    handle_id: u64,
    room_id: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let transaction_id = format!("join-room-{}", chrono::Utc::now().timestamp_millis());
    let join_request = json!({
        "janus": "message",
        "body": {
            "request": "join",
            "ptype": "publisher",
            "room": room_id,
            "display": "Rust Client"
        },
        "transaction": transaction_id,
        "session_id": session_id,
        "handle_id": handle_id
    });

    // Send the join request
    ws_sender
        .send(Message::Text(join_request.to_string()))
        .await?;

    // Wait for the response(s)
    while let Some(msg) = ws_receiver.next().await {
        match msg {
            Ok(Message::Text(response)) => {
                let parsed: Value = serde_json::from_str(&response)?;
                if parsed["transaction"] == transaction_id {
                    if let Some(janus_type) = parsed["janus"].as_str() {
                        match janus_type {
                            "ack" => {
                                // Acknowledgment received, continue waiting for the event
                                info!("Received ack, waiting for event...");
                                continue;
                            }
                            "event" => {
                                // Check if this is the join confirmation
                                if parsed["plugindata"]["data"]["videoroom"] == "joined" {
                                    info!("Successfully joined room with ID: {}", room_id);
                                    return Ok(());
                                } else {
                                    // Handle other events if necessary
                                    let event = parsed["plugindata"]["data"]["videoroom"].as_str().unwrap_or("unknown");
                                    error!("Received unexpected event: {}", event);
                                    return Err(format!("Failed to join room: unexpected event {}", event).into());
                                }
                            }
                            "error" => {
                                // An error occurred
                                let error_code = parsed["error"]["code"].as_i64().unwrap_or(0);
                                let error_reason = parsed["error"]["reason"].as_str().unwrap_or("Unknown error");
                                error!("Failed to join room: {} (code {})", error_reason, error_code);
                                return Err(format!("Failed to join room: {} (code {})", error_reason, error_code).into());
                            }
                            _ => {
                                // Unknown message type
                                error!("Received unknown Janus message: {}", janus_type);
                            }
                        }
                    } else {
                        error!("Received message without 'janus' field: {:?}", parsed);
                    }
                }
            }
            Ok(_) => {
                error!("Received non-text message");
            }
            Err(e) => {
                error!("Error receiving message: {}", e);
                return Err(e.into());
            }
        }
    }

    error!("No response received from Janus");
    Err("Failed to join room".into())
}

async fn send_webrtc_offer(
    ws_sender: Arc<Mutex<SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>>>,
    ws_receiver: Arc<Mutex<SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>>>,
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
            "trickle": true // Enable Trickle ICE
        },
        "jsep": {
            "type": "offer",
            "sdp": offer_sdp
        },
        "transaction": transaction_id,
        "session_id": session_id,
        "handle_id": handle_id
    });

    ws_sender.lock().await.send(Message::Text(offer_message.to_string())).await?;

    while let Some(msg) = ws_receiver.lock().await.next().await {
        if let Ok(Message::Text(response)) = msg {
            let parsed: serde_json::Value = serde_json::from_str(&response)?;
            info!("Received message from Janus: {:?}", parsed);
            if parsed["transaction"] == transaction_id {
                if let Some(janus_type) = parsed["janus"].as_str() {
                    match janus_type {
                        "success" => {
                            if let Some(jsep) = parsed["jsep"].as_object() {
                                if let Some(sdp) = jsep["sdp"].as_str() {
                                    info!("SDP answer received from Janus");
                                    return Ok(sdp.to_string());
                                }
                            }
                        }
                        "ack" => {
                            info!("Received ack, waiting for event...");
                            continue;
                        }
                        "event" => {
                            if let Some(jsep) = parsed["jsep"].as_object() {
                                if let Some(sdp) = jsep["sdp"].as_str() {
                                    info!("SDP answer received from Janus");
                                    return Ok(sdp.to_string());
                                }
                            }
                        }
                        "error" => {
                            let error_code = parsed["error"]["code"].as_i64().unwrap_or(0);
                            let error_reason = parsed["error"]["reason"].as_str().unwrap_or("Unknown error");
                            error!("Failed to send offer: {} (code {})", error_reason, error_code);
                            return Err(format!("Failed to send offer: {} (code {})", error_reason, error_code).into());
                        }
                        _ => {
                            warn!("Received unexpected message type: {:?}", parsed["janus"]);
                        }
                    }
                } else {
                    warn!("Received message without 'janus' field: {:?}", parsed);
                }
            } else {
                // Handle other messages, possibly in another task
                // For now, continue
                continue;
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
    let (ws_stream, _) = connect_async(request).await?;
    println!("WebSocket connection established");

    let (ws_sender, ws_receiver) = ws_stream.split();
    let ws_sender = Arc::new(Mutex::new(ws_sender));
    let ws_receiver = Arc::new(Mutex::new(ws_receiver));

    wait_for_room().await;

    let session_id = create_janus_session(&mut *ws_sender.lock().await, &mut *ws_receiver.lock().await).await?;
    info!("Janus session created with ID: {}", session_id);

    let handle_id = attach_videoroom_plugin(&mut *ws_sender.lock().await, &mut *ws_receiver.lock().await, session_id).await?;
    info!("Attached to VideoRoom plugin with handle ID: {}", handle_id);

    let room_id = *ROOM.get().expect("ROOM should be set");
    join_room(&mut *ws_sender.lock().await, &mut *ws_receiver.lock().await, session_id, handle_id, room_id).await?;
    info!("Joined the room with ID: {}", room_id);

    let pc = Arc::new(create_peer_connection().await);
    add_video_track(Arc::clone(&pc)).await;

    // Set up ICE candidate handling
    {
        let ws_sender_clone = Arc::clone(&ws_sender);
        let session_id = session_id;
        let handle_id = handle_id;
        pc.on_ice_candidate(Box::new(move |candidate: Option<RTCIceCandidate>| {
            let ws_sender_clone = Arc::clone(&ws_sender_clone);
            Box::pin(async move {
                if let Some(candidate) = candidate {
                    // Convert the candidate to JSON
                    let candidate_init = candidate.to_json().unwrap();
                    let candidate_message = json!({
                        "janus": "trickle",
                        "candidate": {
                            "candidate": candidate_init.candidate,
                            "sdpMid": candidate_init.sdp_mid,
                            "sdpMLineIndex": candidate_init.sdp_mline_index,
                        },
                        "transaction": format!("trickle-{}", chrono::Utc::now().timestamp_millis()),
                        "session_id": session_id,
                        "handle_id": handle_id
                    });
                    // Send the candidate to Janus
                    if let Err(err) = ws_sender_clone.lock().await.send(Message::Text(candidate_message.to_string())).await {
                        error!("Failed to send candidate to Janus: {}", err);
                    } else {
                        debug!("Sent local ICE candidate to Janus");
                    }
                } else {
                    // All ICE candidates have been gathered
                    info!("Finished gathering ICE candidates");
                    let completed_message = json!({
                        "janus": "trickle",
                        "candidate": { "completed": true },
                        "transaction": format!("trickle-complete-{}", chrono::Utc::now().timestamp_millis()),
                        "session_id": session_id,
                        "handle_id": handle_id
                    });
                    if let Err(err) = ws_sender_clone.lock().await.send(Message::Text(completed_message.to_string())).await {
                        error!("Failed to send end-of-candidates to Janus: {}", err);
                    } else {
                        debug!("Sent end-of-candidates to Janus");
                    }
                }
            })
        }));
    }

    // Create SDP offer and set local description
    let offer = pc.create_offer(None).await.unwrap();
    info!("SDP Offer:\n{}", offer.sdp);
    pc.set_local_description(offer.clone()).await.unwrap();
    info!("Local SDP offer created and set");

    // Send the offer to Janus and get the SDP answer
    let sdp_answer = send_webrtc_offer(Arc::clone(&ws_sender), Arc::clone(&ws_receiver), session_id, handle_id, offer.sdp).await?;
    info!("SDP Answer:\n{}", sdp_answer);
    pc.set_remote_description(RTCSessionDescription::answer(sdp_answer)?).await.unwrap();
    info!("Remote SDP answer set");

    // Set up ICE connection state change handling
    {
        let pc_clone = Arc::clone(&pc);
        pc.on_ice_connection_state_change(Box::new(move |state| {
            info!("ICE Connection State changed: {:?}", state);
            match state {
                webrtc::ice_transport::ice_connection_state::RTCIceConnectionState::Connected => {
                    info!("ICE Connection established");
                }
                webrtc::ice_transport::ice_connection_state::RTCIceConnectionState::Failed => {
                    error!("ICE Connection failed");
                }
                _ => {}
            }
            Box::pin(async {})
        }));
    }

    // Set up task to handle incoming messages from Janus
    {
        let pc_clone = Arc::clone(&pc);
        let ws_receiver_clone = Arc::clone(&ws_receiver);
        tokio::spawn(async move {
            loop {
                let msg = ws_receiver_clone.lock().await.next().await;
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
                        if let Some(janus_type) = parsed["janus"].as_str() {
                            match janus_type {
                                "trickle" => {
                                    // Handle incoming ICE candidates from Janus
                                    if let Some(candidate) = parsed["candidate"].as_object() {
                                        if let Some(completed) = candidate.get("completed") {
                                            if completed.as_bool() == Some(true) {
                                                info!("Received end-of-candidates from Janus");
                                            }
                                        } else if let Some(candidate_str) = candidate.get("candidate").and_then(|v| v.as_str()) {
                                            let sdp_mid = candidate.get("sdpMid").and_then(|v| v.as_str()).map(|s| s.to_string());
                                            let sdp_mline_index = candidate.get("sdpMLineIndex").and_then(|v| v.as_u64()).map(|i| i as u16);

                                            let ice_candidate = RTCIceCandidateInit {
                                                candidate: candidate_str.to_string(),
                                                sdp_mid,
                                                sdp_mline_index,
                                                ..Default::default()
                                            };

                                            if let Err(err) = pc_clone.add_ice_candidate(ice_candidate).await {
                                                error!("Failed to add ICE candidate: {}", err);
                                            } else {
                                                debug!("Added ICE candidate from Janus");
                                            }
                                        }
                                    }
                                },
                                "webrtcup" => {
                                    info!("WebRTC connection is up");
                                },
                                "hangup" => {
                                    warn!("Received hangup from Janus");
                                    // Handle hangup
                                },
                                "event" => {
                                    // Handle other events
                                    debug!("Received event: {:?}", parsed);
                                },
                                _ => {
                                    debug!("Received other message: {:?}", parsed);
                                }
                            }
                        }
                    },
                    Some(Ok(_)) => {},
                    Some(Err(e)) => {
                        error!("Error reading message from Janus: {}", e);
                        break;
                    },
                    None => {
                        error!("WebSocket stream has terminated");
                        break;
                    }
                }
            }
        });
    }

    // Keep the application running
    loop {
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}
