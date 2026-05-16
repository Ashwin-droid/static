use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::rc::Rc;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use pipewire as pw;
use pw::spa::utils::dict::DictRef;
use pw::types::ObjectType;

macro_rules! eprintln {
    ($($arg:tt)*) => {{
        crate::app_log!($($arg)*);
    }};
}

pub struct PipeWireMonitor {
    active_streams: Arc<Mutex<HashMap<u32, PipeWireStreamInfo>>>,
    _thread: JoinHandle<()>,
}

#[derive(Debug, Clone)]
pub struct PipeWireStreamInfo {
    pub node_id: u32,
    pub first_seen: Instant,
    pub application_id: Option<String>,
    pub application_name: Option<String>,
    pub process_binary: Option<String>,
    pub process_id: Option<String>,
    pub media_name: Option<String>,
    pub node_name: Option<String>,
    pub node_description: Option<String>,
}

impl PipeWireStreamInfo {
    pub fn stable_filter_key(&self) -> Option<String> {
        self.application_id
            .clone()
            .or_else(|| self.application_name.clone())
            .or_else(|| self.process_binary.clone())
            .or_else(|| self.node_description.clone())
            .or_else(|| self.node_name.clone())
            .or_else(|| self.media_name.clone())
    }

    pub fn label(&self) -> String {
        self.application_name
            .clone()
            .or_else(|| self.process_binary.clone())
            .or_else(|| self.node_description.clone())
            .or_else(|| self.node_name.clone())
            .or_else(|| self.media_name.clone())
            .unwrap_or_else(|| "Unidentified PipeWire stream".to_string())
    }

    pub fn detail(&self) -> String {
        let mut parts = vec![format!("node={}", self.node_id)];
        if let Some(application_id) = self.application_id.as_deref() {
            parts.push(format!("app_id={application_id}"));
        }
        if let Some(application_name) = self.application_name.as_deref() {
            parts.push(format!("app={application_name}"));
        }
        if let Some(binary) = self.process_binary.as_deref() {
            parts.push(format!("binary={binary}"));
        }
        if let Some(pid) = self.process_id.as_deref() {
            parts.push(format!("pid={pid}"));
        }
        if let Some(media_name) = self.media_name.as_deref() {
            parts.push(format!("media={media_name}"));
        }
        if let Some(node_name) = self.node_name.as_deref() {
            parts.push(format!("node={node_name}"));
        }
        parts.join(" | ")
    }

    pub fn identifiers(&self) -> Vec<String> {
        [
            self.stable_filter_key(),
            self.application_id.clone(),
            self.application_name.clone(),
            self.process_binary.clone(),
            self.media_name.clone(),
            self.node_name.clone(),
            self.node_description.clone(),
        ]
        .into_iter()
        .flatten()
        .filter(|value| !value.trim().is_empty())
        .collect()
    }
}

struct TrackedNode {
    _node: pw::node::Node,
    _listener: pw::node::NodeListener,
}

impl PipeWireMonitor {
    pub fn new() -> Self {
        let active_streams = Arc::new(Mutex::new(HashMap::new()));
        let active_streams_for_thread = active_streams.clone();
        let thread = thread::spawn(move || run_pipewire_monitor(active_streams_for_thread));

        Self {
            active_streams,
            _thread: thread,
        }
    }

    pub fn active_streams(&self) -> HashMap<u32, PipeWireStreamInfo> {
        lock_streams(&self.active_streams).clone()
    }
}

fn run_pipewire_monitor(active_streams: Arc<Mutex<HashMap<u32, PipeWireStreamInfo>>>) {
    pw::init();

    let self_identifiers = own_process_identifiers();
    let self_pid = std::process::id().to_string();

    let main_loop = match pw::main_loop::MainLoop::new(None) {
        Ok(main_loop) => main_loop,
        Err(err) => {
            eprintln!("[PIPEWIRE] failed to create main loop: {err}");
            clear_streams(&active_streams);
            return;
        }
    };
    let context = match pw::context::Context::new(&main_loop) {
        Ok(context) => context,
        Err(err) => {
            eprintln!("[PIPEWIRE] failed to create context: {err}");
            clear_streams(&active_streams);
            return;
        }
    };
    let core = match context.connect(None) {
        Ok(core) => core,
        Err(err) => {
            eprintln!("[PIPEWIRE] PipeWire unavailable, disabling ducking monitor: {err}");
            clear_streams(&active_streams);
            return;
        }
    };
    let registry = match core.get_registry() {
        Ok(registry) => Rc::new(registry),
        Err(err) => {
            eprintln!("[PIPEWIRE] failed to get registry: {err}");
            clear_streams(&active_streams);
            return;
        }
    };

    let main_loop_weak = main_loop.downgrade();
    let active_streams_for_error = active_streams.clone();
    let tracked_nodes = Rc::new(RefCell::new(HashMap::<u32, TrackedNode>::new()));
    let _core_listener = core
        .add_listener_local()
        .error(move |id, _, res, message| {
            if id == 0 {
                eprintln!(
                    "[PIPEWIRE] core connection error, disabling ducking monitor: {res} ({message})"
                );
                clear_streams(&active_streams_for_error);
                if let Some(main_loop) = main_loop_weak.upgrade() {
                    main_loop.quit();
                }
            }
        })
        .register();

    let registry_weak = Rc::downgrade(&registry);
    let active_streams_for_global = active_streams.clone();
    let active_streams_for_remove = active_streams.clone();
    let tracked_nodes_for_global = tracked_nodes.clone();
    let tracked_nodes_for_remove = tracked_nodes.clone();
    let _registry_listener = registry
        .add_listener_local()
        .global(move |obj| {
            if obj.type_ != ObjectType::Node {
                return;
            }

            let Some(props) = obj.props else {
                return;
            };

            if !is_audio_output_stream(props) || is_own_stream(props, &self_identifiers, &self_pid)
            {
                return;
            }

            let Some(registry) = registry_weak.upgrade() else {
                return;
            };

            let node_id = obj.id;
            let initial_stream = stream_info_from_props(node_id, props);
            let node: pw::node::Node = match registry.bind(obj) {
                Ok(node) => node,
                Err(err) => {
                    eprintln!("[PIPEWIRE] failed to bind node {node_id}: {err}");
                    return;
                }
            };

            let active_streams_for_info = active_streams_for_global.clone();
            let listener = node
                .add_listener_local()
                .info(move |info: &pw::node::NodeInfoRef| {
                    let stream = info
                        .props()
                        .map(|props| stream_info_from_props(node_id, props))
                        .unwrap_or_else(|| initial_stream.clone());
                    update_stream_state(&active_streams_for_info, stream, info.state());
                })
                .register();

            tracked_nodes_for_global.borrow_mut().insert(
                node_id,
                TrackedNode {
                    _node: node,
                    _listener: listener,
                },
            );
        })
        .global_remove(move |id| {
            tracked_nodes_for_remove.borrow_mut().remove(&id);
            lock_streams(&active_streams_for_remove).remove(&id);
        })
        .register();

    main_loop.run();
    clear_streams(&active_streams);
}

fn is_audio_output_stream(props: &DictRef) -> bool {
    props.get("media.class") == Some("Stream/Output/Audio")
}

fn is_own_stream(props: &DictRef, self_identifiers: &HashSet<String>, self_pid: &str) -> bool {
    if props
        .get("application.process.id")
        .is_some_and(|pid| pid.trim() == self_pid)
    {
        return true;
    }

    props
        .get("application.name")
        .into_iter()
        .chain(props.get("application.process.binary"))
        .any(|value| matches_own_identifier(value, self_identifiers))
}

fn stream_info_from_props(node_id: u32, props: &DictRef) -> PipeWireStreamInfo {
    PipeWireStreamInfo {
        node_id,
        first_seen: Instant::now(),
        application_id: dict_string(props, "application.id"),
        application_name: dict_string(props, "application.name"),
        process_binary: dict_string(props, "application.process.binary"),
        process_id: dict_string(props, "application.process.id"),
        media_name: dict_string(props, "media.name"),
        node_name: dict_string(props, "node.name"),
        node_description: dict_string(props, "node.description"),
    }
}

fn dict_string(props: &DictRef, key: &str) -> Option<String> {
    props
        .get(key)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn matches_own_identifier(value: &str, self_identifiers: &HashSet<String>) -> bool {
    let normalized = normalize_identifier(value);
    if self_identifiers.contains(&normalized) {
        return true;
    }

    Path::new(value)
        .file_name()
        .and_then(|name| name.to_str())
        .map(normalize_identifier)
        .is_some_and(|name| self_identifiers.contains(&name))
}

fn own_process_identifiers() -> HashSet<String> {
    let mut identifiers = HashSet::new();

    if let Ok(exe) = std::env::current_exe() {
        if let Some(name) = exe.file_name().and_then(|name| name.to_str()) {
            identifiers.insert(normalize_identifier(name));
        }
        if let Some(stem) = exe.file_stem().and_then(|stem| stem.to_str()) {
            identifiers.insert(normalize_identifier(stem));
        }
    }

    identifiers.insert(normalize_identifier(env!("CARGO_PKG_NAME")));
    identifiers.insert(normalize_identifier("Static"));
    identifiers.retain(|value| !value.is_empty());
    identifiers
}

fn normalize_identifier(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

fn update_stream_state(
    active_streams: &Arc<Mutex<HashMap<u32, PipeWireStreamInfo>>>,
    mut stream: PipeWireStreamInfo,
    state: pw::node::NodeState<'_>,
) {
    let mut active_streams = lock_streams(active_streams);

    match state {
        pw::node::NodeState::Running => {
            if let Some(existing) = active_streams.get(&stream.node_id) {
                stream.first_seen = existing.first_seen;
            }
            active_streams.insert(stream.node_id, stream);
        }
        pw::node::NodeState::Creating
        | pw::node::NodeState::Suspended
        | pw::node::NodeState::Idle
        | pw::node::NodeState::Error(_) => {
            active_streams.remove(&stream.node_id);
        }
    }
}

fn clear_streams(active_streams: &Arc<Mutex<HashMap<u32, PipeWireStreamInfo>>>) {
    lock_streams(active_streams).clear();
}

fn lock_streams(
    active_streams: &Arc<Mutex<HashMap<u32, PipeWireStreamInfo>>>,
) -> MutexGuard<'_, HashMap<u32, PipeWireStreamInfo>> {
    active_streams
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}
