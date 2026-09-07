//! Ephemeral, loopback-only HTTPS host for the bundled visualiser.
//! D-Bus calls run on a bounded worker; the socket only retains its latest frame.
use std::{
    collections::HashMap,
    net::TcpListener,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use axum::{
    Router,
    body::Body,
    extract::{
        Path, Query, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{HeaderMap, StatusCode, Uri, header},
    response::{IntoResponse, Response},
    routing::get,
};
use futures_util::{SinkExt, StreamExt};
use include_dir::{Dir, include_dir};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{Semaphore, watch};
use tokio_util::io::ReaderStream;

use crate::ipc::DaemonClient;

static ASSETS: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/visualizer/dist");
const MAX_MESSAGE: usize = 4 * 1024 * 1024;
const FRAME_INTERVAL: Duration = Duration::from_millis(33);

pub struct Server {
    pub url: String,
    pub certificate_pem: String,
    shutdown: axum_server::Handle,
    stopping: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        self.shutdown.shutdown();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[derive(Clone)]
struct AppState {
    origin: String,
    host: String,
    token: String,
    stopping: Arc<AtomicBool>,
    exports: Arc<Mutex<HashMap<String, (super::cartridge::CartridgeStorage, String)>>>,
    export_slot: Arc<Semaphore>,
    sampling_enabled: bool,
    connections: Arc<Semaphore>,
}

pub fn start() -> Result<Server> {
    start_inner(true)
}

fn start_inner(sampling_enabled: bool) -> Result<Server> {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
    listener.set_nonblocking(true)?;
    let host = listener.local_addr()?.to_string();
    let origin = format!("https://{host}");
    let token = uuid::Uuid::new_v4().to_string();
    let cert = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()])?;
    let certificate_pem = cert.cert.pem();
    let private_key = cert.signing_key.serialize_pem();
    let shutdown = axum_server::Handle::new();
    let stopping = Arc::new(AtomicBool::new(false));
    let state = AppState {
        origin: origin.clone(),
        host,
        token: token.clone(),
        stopping: stopping.clone(),
        exports: Arc::new(Mutex::new(HashMap::new())),
        export_slot: Arc::new(Semaphore::new(1)),
        sampling_enabled,
        connections: Arc::new(Semaphore::new(4)),
    };
    let thread_shutdown = shutdown.clone();
    let thread_cert = certificate_pem.clone();
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let thread = thread::Builder::new()
        .name("visualiser-https".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = ready_tx.send(Err(error.to_string()));
                    return;
                }
            };
            runtime.block_on(async move {
                // Explicit provider avoids global-provider ambiguity with other TLS clients.
                let _ = rustls::crypto::ring::default_provider().install_default();
                let tls = match axum_server::tls_rustls::RustlsConfig::from_pem(
                    thread_cert.into_bytes(),
                    private_key.into_bytes(),
                )
                .await
                {
                    Ok(tls) => tls,
                    Err(error) => {
                        let _ = ready_tx.send(Err(error.to_string()));
                        return;
                    }
                };
                let server = axum_server::from_tcp_rustls(listener, tls);
                let _ = ready_tx.send(Ok(()));
                if let Err(error) = server
                    .handle(thread_shutdown)
                    .serve(router(state).into_make_service())
                    .await
                {
                    tracing::warn!(%error, "Visualiser HTTPS server stopped");
                }
            });
            runtime.shutdown_timeout(Duration::from_millis(500));
        })?;
    ready_rx
        .recv()
        .context("Visualiser server stopped during startup")?
        .map_err(anyhow::Error::msg)?;
    Ok(Server {
        url: format!("{origin}/#token={token}"),
        certificate_pem,
        shutdown,
        stopping,
        thread: Some(thread),
    })
}

fn router(state: AppState) -> Router {
    Router::new()
        .route("/ws", get(upgrade))
        .route("/cartridges/{id}", get(download))
        .fallback(asset)
        .with_state(state)
}

fn valid_host(headers: &HeaderMap, uri: &Uri, state: &AppState) -> bool {
    // HTTP/1 normally carries Host; HTTP/2 carries :authority in the URI.
    // When both exist, every supplied value must name this exact listener.
    let mut supplied = false;
    for host in headers.get_all(header::HOST) {
        supplied = true;
        if host.to_str().ok() != Some(state.host.as_str()) {
            return false;
        }
    }
    if let Some(authority) = uri.authority() {
        supplied = true;
        if authority.as_str() != state.host {
            return false;
        }
    }
    supplied
}

#[derive(Deserialize)]
struct Auth {
    token: Option<String>,
}

fn authenticated(
    headers: &HeaderMap,
    uri: &Uri,
    auth: &Auth,
    state: &AppState,
    require_origin: bool,
) -> bool {
    valid_host(headers, uri, state)
        && auth.token.as_deref() == Some(state.token.as_str())
        && (!require_origin
            || headers.get(header::ORIGIN).and_then(|v| v.to_str().ok())
                == Some(state.origin.as_str()))
}

async fn upgrade(
    State(state): State<AppState>,
    Query(auth): Query<Auth>,
    headers: HeaderMap,
    uri: Uri,
    ws: WebSocketUpgrade,
) -> Response {
    if !authenticated(&headers, &uri, &auth, &state, true) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Ok(permit) = state.connections.clone().try_acquire_owned() else {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    };
    ws.max_message_size(MAX_MESSAGE)
        .max_frame_size(MAX_MESSAGE)
        .on_upgrade(move |socket| async move {
            let _permit = permit;
            connection(socket, state).await;
        })
}

fn asset_path(path: &str) -> Option<&str> {
    let path = path.strip_prefix('/')?;
    if path.is_empty() {
        return Some("index.html");
    }
    if path.contains(['%', '\\'])
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return None;
    }
    Some(path)
}

async fn asset(State(state): State<AppState>, headers: HeaderMap, uri: Uri) -> Response {
    if !valid_host(&headers, &uri, &state) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Some(file) = asset_path(uri.path()).and_then(|path| ASSETS.get_file(path)) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mime = match file.path().extension().and_then(|ext| ext.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("woff2") => "font/woff2",
        Some("ico") => "image/x-icon",
        Some("json") => "application/json",
        _ => "application/octet-stream",
    };
    let mut response = (
        [
            (header::CONTENT_TYPE, mime),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        file.contents(),
    )
        .into_response();
    // WebKit versions differ on whether 'self' includes a wss scheme upgrade.
    let policy = format!(
        "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; font-src 'self' data:; connect-src 'self' wss://{}; media-src 'none'; object-src 'none'; frame-ancestors 'none'; base-uri 'none'",
        state.host
    );
    response.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        policy.parse().expect("loopback host is a valid header"),
    );
    response
}

async fn download(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(auth): Query<Auth>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    if !authenticated(&headers, &uri, &auth, &state, false) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let entry = state
        .exports
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&id)
        .cloned();
    let Some((storage, filename)) = entry else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let body = match storage {
        super::cartridge::CartridgeStorage::Disk(path) => {
            let Ok(file) = tokio::fs::File::open(path).await else {
                return StatusCode::NOT_FOUND.into_response();
            };
            Body::from_stream(ReaderStream::new(file))
        }
        super::cartridge::CartridgeStorage::Memory { name, size } => {
            match memory_download(name, size).await {
                Ok(body) => body,
                Err(status) => return status.into_response(),
            }
        }
    };
    let disposition = format!(
        "attachment; filename=\"{}\"",
        filename
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || "._-".contains(*c))
            .collect::<String>()
    );
    (
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_owned()),
            (header::CONTENT_DISPOSITION, disposition),
            (header::CACHE_CONTROL, "no-store".to_owned()),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_owned()),
        ],
        body,
    )
        .into_response()
}

/// D-Bus uses its own native worker, never a Tokio blocking/runtime thread.
/// A slow browser can retain at most two chunks in this channel.
async fn memory_download(name: String, size: u64) -> std::result::Result<Body, StatusCode> {
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (chunks_tx, chunks_rx) = tokio::sync::mpsc::channel::<std::io::Result<Vec<u8>>>(2);
    thread::Builder::new()
        .name("cartridge-download".into())
        .spawn(move || {
            let client = match DaemonClient::connect_existing() {
                Ok(client) => client,
                Err(_) => {
                    let _ = ready_tx.send(Err(StatusCode::SERVICE_UNAVAILABLE));
                    return;
                }
            };
            match client.try_memory_cartridge_info(&name) {
                Ok(Some(info)) if info.size == size => {}
                Ok(_) => {
                    let _ = ready_tx.send(Err(StatusCode::NOT_FOUND));
                    return;
                }
                Err(_) => {
                    let _ = ready_tx.send(Err(StatusCode::SERVICE_UNAVAILABLE));
                    return;
                }
            }
            if ready_tx.send(Ok(())).is_err() {
                return;
            }
            stream_memory_chunks(
                size,
                |offset, count| {
                    client
                        .try_read_memory_cartridge(&name, offset, count)
                        .map_err(std::io::Error::other)?
                        .ok_or_else(|| {
                            std::io::Error::new(
                                std::io::ErrorKind::NotFound,
                                "Cartridge is no longer in memory",
                            )
                        })
                },
                |chunk| chunks_tx.blocking_send(chunk).is_ok(),
            );
        })
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    ready_rx
        .await
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)??;
    let stream = futures_util::stream::unfold(chunks_rx, |mut receiver| async move {
        receiver.recv().await.map(|chunk| (chunk, receiver))
    });
    Ok(Body::from_stream(stream))
}

fn stream_memory_chunks(
    size: u64,
    mut read: impl FnMut(u64, u32) -> std::io::Result<Vec<u8>>,
    mut send: impl FnMut(std::io::Result<Vec<u8>>) -> bool,
) {
    let mut offset = 0;
    while offset < size {
        let count = (size - offset).min(super::cartridge::STREAM_CHUNK_BYTES as u64) as u32;
        let result = read(offset, count).and_then(|bytes| {
            if bytes.len() == count as usize {
                Ok(bytes)
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "Incomplete cartridge read",
                ))
            }
        });
        let failed = result.is_err();
        if !send(result) || failed {
            return;
        }
        offset += u64::from(count);
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Command {
    Configure {
        bands: u32,
    },
    Transport {
        action: Transport,
    },
    Volume {
        value: f64,
    },
    Cartridge {
        id: String,
        video_id: String,
        screenshot: String,
        metadata: Value,
    },
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Transport {
    Toggle,
    Next,
    Previous,
}

fn validate_command(command: &Command) -> Result<()> {
    match command {
        Command::Configure { bands } => anyhow::ensure!(
            (8..=128).contains(bands),
            "Frequency bands must be between 8 and 128"
        ),
        Command::Volume { value } => anyhow::ensure!(
            value.is_finite() && (0.0..=5.0).contains(value),
            "Volume must be between 0 and 5"
        ),
        Command::Cartridge {
            id,
            video_id,
            screenshot,
            ..
        } => {
            anyhow::ensure!(
                !id.is_empty()
                    && id.len() <= 80
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b)),
                "Invalid request ID"
            );
            anyhow::ensure!(
                !video_id.is_empty()
                    && video_id.len() <= 64
                    && video_id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b)),
                "Invalid video ID"
            );
            anyhow::ensure!(
                screenshot.starts_with("data:image/png;base64,"),
                "Screenshot must be a PNG data URL"
            );
        }
        _ => {}
    }
    Ok(())
}

fn error(message: impl std::fmt::Display) -> String {
    json!({"type":"error", "message":message.to_string()}).to_string()
}

// State and theme watches are independently coalesced. A newer daemon state
// always wins over a theme still queued from the previous song.
fn theme_matches_state(theme: &str, state: &str) -> bool {
    let Ok(theme) = serde_json::from_str::<Value>(theme) else {
        return false;
    };
    let Ok(state) = serde_json::from_str::<Value>(state) else {
        return false;
    };
    theme["type"] == "theme"
        && state["type"] == "state"
        && theme["video_id"] == state["state"]["current_track"]["video_id"]
}

struct WorkerStop(Arc<AtomicBool>);
impl Drop for WorkerStop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

fn worker(
    commands: mpsc::Receiver<Command>,
    audio: watch::Sender<String>,
    state: watch::Sender<String>,
    theme: watch::Sender<String>,
    stop: Arc<AtomicBool>,
    server_stop: Arc<AtomicBool>,
) {
    let cover = super::theme::ThemeWorker::start(theme.clone()).ok();
    let mut client: Option<DaemonClient> = None;
    let mut bands = 64;
    let mut last_state = Instant::now() - Duration::from_secs(1);
    let mut next_connect = Instant::now();
    while !stop.load(Ordering::Acquire) && !server_stop.load(Ordering::Acquire) {
        let start = Instant::now();
        if client.is_none() && start >= next_connect {
            match DaemonClient::connect_existing() {
                Ok(connected) => {
                    client = Some(connected);
                    last_state = start - Duration::from_secs(1);
                }
                Err(err) => {
                    state.send_replace(error(format!("Playback daemon unavailable: {err}")));
                }
            }
            next_connect = start + Duration::from_secs(1);
        }
        // A bounded input channel prevents browser commands starving audio polling.
        for _ in 0..8 {
            let Ok(command) = commands.try_recv() else {
                break;
            };
            if let Command::Configure { bands: requested } = command {
                bands = requested;
                continue;
            }
            let Some(daemon) = client.as_ref() else {
                state.send_replace(error("Playback daemon unavailable"));
                continue;
            };
            let result = match command {
                Command::Transport {
                    action: Transport::Toggle,
                } => daemon.toggle_pause(),
                Command::Transport {
                    action: Transport::Next,
                } => daemon.next(),
                Command::Transport {
                    action: Transport::Previous,
                } => daemon.previous(),
                Command::Volume { value } => daemon.set_volume(value),
                _ => continue,
            };
            match result {
                Ok(snapshot) => {
                    state.send_replace(json!({"type":"state", "state":snapshot}).to_string());
                    if let Some(cover) = &cover {
                        cover.update(snapshot.current_track.as_ref());
                    }
                }
                Err(err) => {
                    state.send_replace(error(err));
                }
            }
        }
        if let Some(daemon) = client.as_ref() {
            let result = (|| -> Result<()> {
                if last_state.elapsed() >= Duration::from_millis(250) {
                    let snapshot = daemon.state()?;
                    state.send_replace(json!({"type":"state", "state":snapshot}).to_string());
                    if let Some(cover) = &cover {
                        cover.update(snapshot.current_track.as_ref());
                    }
                    last_state = Instant::now();
                }
                let frame = daemon.visualization_frame(bands)?;
                audio.send_replace(json!({"type":"audio", "frame":frame}).to_string());
                Ok(())
            })();
            if let Err(err) = result {
                client = None;
                if let Some(cover) = &cover {
                    cover.update(None);
                }
                audio.send_replace(String::new());
                state.send_replace(error(format!("Playback daemon disconnected: {err}")));
                next_connect = Instant::now() + Duration::from_secs(1);
            }
        }
        thread::sleep(FRAME_INTERVAL.saturating_sub(start.elapsed()));
    }
}

/// Secret Service and D-Bus use synchronous runtimes internally. A Tokio
/// blocking worker still carries a Tokio Handle, so keep the entire export
/// (including connection destruction) on a thread with no inherited runtime.
async fn export_thread<T, F>(work: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    let (sender, receiver) = tokio::sync::oneshot::channel();
    thread::Builder::new()
        .name("visualiser-export".into())
        .spawn(move || {
            let _ = sender.send(work());
        })
        .context("Cannot start native cartridge worker")?;
    receiver
        .await
        .context("Native cartridge worker stopped before completion")?
}

async fn connection(socket: WebSocket, app: AppState) {
    let (mut sink, mut stream) = socket.split();
    let (commands_tx, commands_rx) = mpsc::sync_channel(8);
    let (audio_tx, mut audio_rx) = watch::channel(String::new());
    let (state_tx, mut state_rx) = watch::channel(String::new());
    let (theme_tx, mut theme_rx) = watch::channel(String::new());
    let (export_tx, mut export_rx) = tokio::sync::mpsc::channel::<String>(2);
    let stop = Arc::new(AtomicBool::new(false));
    let _stop = WorkerStop(stop.clone());
    let export_stop = stop.clone();
    let server_stop = app.stopping.clone();
    let _idle_channels = if app.sampling_enabled {
        if thread::Builder::new()
            .name("visualiser-daemon".into())
            .spawn(move || worker(commands_rx, audio_tx, state_tx, theme_tx, stop, server_stop))
            .is_err()
        {
            return;
        }
        None
    } else {
        Some((commands_rx, audio_tx, state_tx, theme_tx))
    };
    let mut shutdown_check = tokio::time::interval(Duration::from_millis(100));
    loop {
        let outgoing = tokio::select! {
            _ = shutdown_check.tick() => { if app.stopping.load(Ordering::Acquire) { break; } continue; }
            changed = audio_rx.changed() => { if changed.is_err() { break; } let message = audio_rx.borrow_and_update().clone(); if message.is_empty() { continue; } message }
            changed = theme_rx.changed() => {
                if changed.is_err() { break; }
                let message = theme_rx.borrow_and_update().clone();
                if message.is_empty() { continue; }
                // Independent watch channels may become ready together. Deliver
                // the latest snapshot first so clients can filter themes by track.
                let latest_state = state_rx.borrow().clone();
                if !theme_matches_state(&message, &latest_state) { continue; }
                if !latest_state.is_empty() && !matches!(tokio::time::timeout(
                    Duration::from_secs(2), sink.send(Message::Text(latest_state.into()))
                ).await, Ok(Ok(()))) { break; }
                message
            }
            changed = state_rx.changed() => { if changed.is_err() { break; } state_rx.borrow_and_update().clone() }
            Some(message) = export_rx.recv() => message,
            incoming = stream.next() => {
                let Some(Ok(incoming)) = incoming else { break; };
                match incoming {
                    Message::Text(text) => {
                        match serde_json::from_str::<Command>(&text).map_err(anyhow::Error::from).and_then(|command| { validate_command(&command)?; Ok(command) }) {
                            Err(err) => error(err),
                            Ok(Command::Cartridge { id, video_id, screenshot, metadata }) => {
                                match app.export_slot.clone().try_acquire_owned() {
                                    Err(_) => json!({"type":"error", "id":id, "message":"A cartridge export is already running"}).to_string(),
                                    Ok(permit) => {
                                        let app = app.clone(); let output = export_tx.clone();
                                        let cancelled = export_stop.clone();
                                        tokio::spawn(async move {
                                            let export_id = id.clone();
                                            let result = export_thread(move || {
                                                let _permit = permit;
                                                anyhow::ensure!(!cancelled.load(Ordering::Acquire), "Cartridge export cancelled");
                                                tracing::debug!("Resolving native cartridge source");
                                                let source = {
                                                    let daemon = DaemonClient::connect_existing()?;
                                                    daemon.visualization_source(&video_id)?
                                                };
                                                anyhow::ensure!(!cancelled.load(Ordering::Acquire), "Cartridge export cancelled");
                                                tracing::debug!("Rendering native cartridge and copying audio");
                                                let file = super::cartridge::create(&source, &screenshot, metadata)?;
                                                tracing::debug!("Native cartridge completed");
                                                Ok(file)
                                            }).await;
                                            let message = match result {
                                                Ok(file) => {
                                                    let key = uuid::Uuid::new_v4().to_string();
                                                    let url = format!("{}/cartridges/{}?token={}", app.origin, key, app.token);
                                                    let mut registry = app.exports.lock().unwrap_or_else(|e| e.into_inner());
                                                    if registry.len() >= 128 { if let Some(old) = registry.keys().next().cloned() { registry.remove(&old); } }
                                                    registry.insert(key, (file.storage.clone(), file.filename.clone()));
                                                    json!({"type":"cartridge_ready", "id":export_id, "filename":file.filename, "url":url, "path":file.display_path, "storage":file.storage.kind()}).to_string()
                                                }
                                                Err(err) => json!({"type":"error", "id":export_id, "message":err.to_string()}).to_string(),
                                            };
                                            let _ = output.send(message).await;
                                        });
                                        continue;
                                    }
                                }
                            }
                            Ok(command) => match commands_tx.try_send(command) { Ok(()) => continue, Err(_) => error("Too many pending playback commands") }
                        }
                    }
                    Message::Close(_) => break,
                    Message::Ping(payload) => { if sink.send(Message::Pong(payload)).await.is_err() { break; } continue; }
                    Message::Pong(_) => continue,
                    Message::Binary(_) => error("Expected a JSON text message"),
                }
            }
        };
        // Slow renderers drop intermediate analysis frames through the watch channel.
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

#[cfg(test)]
#[path = "../tests/visualizer_server.rs"]
mod tests;

#[cfg(test)]
mod theme_delivery_tests {
    use super::*;

    #[test]
    fn theme_delivery_requires_its_current_track_snapshot() {
        let state =
            json!({"type":"state", "state":{"current_track":{"video_id":"new"}}}).to_string();
        assert!(!theme_matches_state(
            &json!({"type":"theme","video_id":"old","theme":null}).to_string(),
            &state
        ));
        assert!(theme_matches_state(
            &json!({"type":"theme","video_id":"new","theme":null}).to_string(),
            &state
        ));
        assert!(!theme_matches_state("{}", &state));
        assert!(!theme_matches_state("invalid", &state));
        assert!(theme_matches_state(
            &json!({"type":"theme","video_id":null,"theme":null}).to_string(),
            &json!({"type":"state","state":{"current_track":null}}).to_string()
        ));
    }
}
