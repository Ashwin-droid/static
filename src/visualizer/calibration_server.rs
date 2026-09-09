//! Separate low-rate diagnostics subscription; no FFT or cover-art work.
use super::*;
use crate::ipc::GainCalibrationSettings;

pub(super) async fn upgrade(
    State(app): State<AppState>,
    Query(auth): Query<Auth>,
    headers: HeaderMap,
    uri: Uri,
    ws: WebSocketUpgrade,
) -> Response {
    if !authenticated(&headers, &uri, &auth, &app, true) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Ok(permit) = app.connections.clone().try_acquire_owned() else {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    };
    ws.max_message_size(4096)
        .max_frame_size(4096)
        .on_upgrade(move |socket| async move {
            let _permit = permit;
            connection(socket, app).await;
        })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Edit {
    settings: GainCalibrationSettings,
}

async fn connection(socket: WebSocket, app: AppState) {
    let (mut sink, mut stream) = socket.split();
    let (edits_tx, edits_rx) = mpsc::sync_channel::<GainCalibrationSettings>(1);
    let (frames_tx, mut frames_rx) = watch::channel(String::new());
    let (replies_tx, mut replies_rx) = tokio::sync::mpsc::channel::<String>(2);
    let stop = Arc::new(AtomicBool::new(false));
    let _stop = WorkerStop(stop.clone());
    let stopping = app.stopping.clone();
    if thread::Builder::new().name("dgain-calibration".into()).spawn(move || {
        let mut client: Option<DaemonClient> = None;
        let mut next_connect = Instant::now();
        while !stop.load(Ordering::Acquire) && !stopping.load(Ordering::Acquire) {
            if client.is_none() && Instant::now() >= next_connect {
                client = DaemonClient::connect_existing().ok();
                next_connect = Instant::now() + Duration::from_secs(1);
            }
            if let Some(edit) = edits_rx.try_iter().next() {
                let reply = match client.as_ref() {
                    Some(client) => match client.set_gain_calibration(edit) {
                        Ok(_) => json!({"type":"saved"}),
                        Err(error) => json!({"type":"error","message":error.to_string()}),
                    },
                    None => json!({"type":"error","message":"Playback daemon unavailable"}),
                };
                let _ = replies_tx.try_send(reply.to_string());
            }
            let result = client.as_ref().context("Playback daemon unavailable").and_then(|client| {
                let frame = client.gain_diagnostics()?;
                Ok(json!({"type":"diagnostics", "frame":frame, "paused":frame.playback_paused,
                    "track":frame.track.as_ref().map(|track| json!({"video_id":track.video_id,"title":track.title,"artist":track.artist}))}))
            });
            match result {
                Ok(value) => { frames_tx.send_replace(value.to_string()); },
                Err(error) => { frames_tx.send_replace(json!({"type":"unavailable","message":error.to_string()}).to_string()); client = None; },
            }
            thread::sleep(Duration::from_millis(100));
        }
    }).is_err() { return; }
    let mut shutdown = tokio::time::interval(Duration::from_millis(200));
    loop {
        let outgoing = tokio::select! {
            _ = shutdown.tick() => { if app.stopping.load(Ordering::Acquire) { break; } continue; },
            changed = frames_rx.changed() => { if changed.is_err() { break; } let value=frames_rx.borrow_and_update().clone(); if value.is_empty() { continue; } value },
            Some(reply) = replies_rx.recv() => reply,
            incoming = stream.next() => {
                match incoming {
                    Some(Ok(Message::Text(raw))) => match serde_json::from_str::<Edit>(&raw) {
                        Ok(edit) => match edits_tx.try_send(edit.settings) { Ok(()) => continue, Err(_) => error("A settings update is already pending") },
                        Err(_) => error("Invalid dGain settings message"),
                    },
                    Some(Ok(Message::Ping(data))) => { if sink.send(Message::Pong(data)).await.is_err() { break; } continue; },
                    Some(Ok(Message::Pong(_))) => continue,
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    _ => error("Expected dGain settings as JSON"),
                }
            }
        };
        if !matches!(
            tokio::time::timeout(
                Duration::from_secs(2),
                sink.send(Message::Text(outgoing.into()))
            )
            .await,
            Ok(Ok(()))
        ) {
            break;
        }
    }
}
