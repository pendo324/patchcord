//! Native PipeWire backend, replacing `pw-dump`/`pw-link`/`pactl` shelling
//! with direct `libpipewire` API calls via the `pipewire` crate.
//!
//! PipeWire objects (MainLoop/Context/Core/Registry/proxies) are not
//! `Send`/`Sync`, so they must all live on one dedicated thread. This module
//! runs that thread and exposes a synchronous-looking `PipewireBackend`
//! handle to the rest of patchcord (in particular, the existing stdio
//! JSON-RPC loop in `main.rs`), using `pipewire::channel` to send commands
//! in and `std::sync::mpsc` to send responses/events back out.

use pipewire::{
    context::ContextRc,
    core::PW_ID_CORE,
    keys,
    link::Link,
    node::Node,
    properties::properties,
    types::ObjectType,
    main_loop::MainLoopRc,
};
use std::collections::HashMap;
use std::io::{Cursor, Seek, Write};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use super::error::{BackendError, Result};
use super::models::{NodeRecord, PortDirection, PortRecord};
use crate::logger;

/// Serialization helpers for building SPA pods needed by the backend.
mod decode {
    use super::*;
    use libspa::pod::serialize::{PodSerialize, PodSerializer, SerializeSuccess};

    /// A SPA Props object with a single `sink` mute property, serialized the
    /// same way libspa's own pod tests build Props objects. `SPA_PROP_mute`
    /// is 0x10000 + 4 per spa/param/props.h (Audio section, after volume).
    pub struct MuteProps(pub bool);

    impl PodSerialize for MuteProps {
        fn serialize<O: Write + Seek>(
            &self,
            serializer: PodSerializer<O>,
        ) -> std::result::Result<SerializeSuccess<O>, cookie_factory::GenError> {
            let mut obj = serializer.serialize_object(
                libspa::sys::SPA_TYPE_OBJECT_Props,
                libspa::sys::SPA_PARAM_Props,
            )?;
            obj.serialize_property(
                libspa::sys::SPA_PROP_mute,
                &self.0,
                libspa::pod::PropertyFlags::empty(),
            )?;
            obj.end()
        }
    }

    /// Writes the serialized mute-props pod bytes into `out`.
    pub fn serialize_mute_props<O: Write + Seek>(
        out: O,
        mute: bool,
    ) -> std::result::Result<(), cookie_factory::GenError> {
        PodSerializer::serialize(out, &MuteProps(mute)).map(|_| ())
    }
}


/// Monotonic handle counter for objects we create on the PipeWire thread.
/// Handles identify *our* proxies (keyed in created_nodes/created_links),
/// independent of the server-assigned registry global ids.
fn next_handle() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT_HANDLE: AtomicU32 = AtomicU32::new(1);
    NEXT_HANDLE.fetch_add(1, Ordering::Relaxed)
}

/// Commands sent from the JSON-RPC thread into the PipeWire loop thread.
pub enum Command {
    /// Snapshot the current live graph state.
    Snapshot { reply: mpsc::Sender<GraphSnapshot> },
    /// Create a link between an output port and an input port. The reply
    /// is a monotonic *handle* for the created proxy, NOT the PipeWire
    /// registry id (which isn't reliably available from the create
    /// result). Callers resolve the real id from `GraphSnapshot` when they
    /// need it.
    CreateLink {
        output_node: u32,
        output_port: u32,
        input_node: u32,
        input_port: u32,
        reply: mpsc::Sender<Result<u32>>,
    },
    /// Destroy a previously-created link by its handle.
    DestroyLink { link_handle: u32, reply: mpsc::Sender<Result<()>> },
    /// Create a virtual sink or source node via the native null-audio-sink
    /// factory (replaces `pactl load-module module-null-sink`/
    /// `module-remap-source`). Reply is likewise a monotonic handle.
    CreateVirtualNode {
        node_name: String,
        node_description: String,
        media_class: &'static str,
        reply: mpsc::Sender<Result<u32>>,
    },
    /// Destroy a previously-created virtual node by its handle.
    DestroyNode { node_handle: u32, reply: mpsc::Sender<Result<()>> },
    /// Mute or unmute a node (identified by its registry id resolved from
    /// a snapshot) via its native `Props` parameter, replacing
    /// `pactl set-source-mute`.
    SetMute { node_id: u32, mute: bool, reply: mpsc::Sender<Result<()>> },
    Shutdown,
}

/// Events sent from the PipeWire loop thread out to the JSON-RPC thread,
/// for things the loop thread notices asynchronously (graph changes,
/// default sink changes) rather than in direct response to a command.
pub enum BackendEvent {
    GraphChanged,
    DefaultSinkChanged(Option<String>),
}

#[derive(Debug, Clone, Default)]
pub struct GraphSnapshot {
    pub nodes: HashMap<u32, NodeRecord>,
    /// (output_node_id, output_port_id) -> Vec<(input_node_id, input_port_id)>
    /// links, keyed by link id, so callers can determine which nodes a
    /// given node's output ports are currently connected to. This is what
    /// backs the `onlySpeakers`/`onlyDefaultSpeakers` filters: rather than
    /// filtering on static node properties, they ask "does this node's
    /// existing output already terminate at a real speaker / the default
    /// speaker?" by walking this map.
    pub links: HashMap<u32, LinkRecord>,
    pub default_sink_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LinkRecord {
    pub output_node: u32,
    pub output_port: u32,
    pub input_node: u32,
    pub input_port: u32,
}

impl GraphSnapshot {
    /// Node ids that at least one of `node_id`'s current output ports is
    /// linked to. Used by the only-speakers / only-default-speakers filters
    /// to inspect where a node's audio is *actually* currently routed,
    /// mirroring venmic's own `should_link()` logic.
    pub fn link_targets_of(&self, node_id: u32) -> Vec<u32> {
        self.links
            .values()
            .filter(|link| link.output_node == node_id)
            .map(|link| link.input_node)
            .collect()
    }
}

/// A handle to the running PipeWire loop thread. Cloneable/cheap; the
/// actual PipeWire objects never leave the dedicated thread.
pub struct PipewireBackend {
    command_tx: pipewire::channel::Sender<Command>,
    _thread: thread::JoinHandle<()>,
}

impl PipewireBackend {
    /// Spawns the dedicated PipeWire loop thread and connects to the
    /// server. `event_tx` receives asynchronous graph/default-sink change
    /// notifications for as long as the backend is alive.
    pub fn spawn(event_tx: mpsc::Sender<BackendEvent>) -> Result<Self> {
        let (command_tx, command_rx) = pipewire::channel::channel::<Command>();

        let thread = thread::Builder::new()
            .name("patchcord-pipewire".to_string())
            .spawn(move || {
                if let Err(err) = run_loop(command_rx, event_tx) {
                    logger::warn(&format!("[pipewire] loop thread exited with error: {err}"));
                }
            })
            .map_err(|err| BackendError::Message(format!("failed to spawn pipewire thread: {err}")))?;

        Ok(Self {
            command_tx,
            _thread: thread,
        })
    }

    pub fn snapshot(&self) -> Result<GraphSnapshot> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.send(Command::Snapshot { reply: reply_tx })?;
        reply_rx
            .recv()
            .map_err(|_| BackendError::Message("pipewire thread did not respond".to_string()))
    }

    /// Blocks (with small sleeps) until the registry's initial burst of
    /// globals has been received, so callers don't observe a spuriously
    /// empty graph in the first few milliseconds after spawn. Returns
    /// whatever state we have once nodes are visible or the timeout hits.
    pub fn wait_ready(&self, timeout: Duration) -> GraphSnapshot {
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(snapshot) = self.snapshot()
                && !snapshot.nodes.is_empty()
            {
                return snapshot;
            }
            if Instant::now() >= deadline {
                return self.snapshot().unwrap_or_default();
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    pub fn create_link(&self, output_node: u32, output_port: u32, input_node: u32, input_port: u32) -> Result<u32> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.send(Command::CreateLink {
            output_node,
            output_port,
            input_node,
            input_port,
            reply: reply_tx,
        })?;
        recv_result(reply_rx)
    }

    pub fn destroy_link(&self, link_handle: u32) -> Result<()> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.send(Command::DestroyLink { link_handle, reply: reply_tx })?;
        recv_result(reply_rx)
    }

    pub fn create_virtual_node(&self, node_name: String, node_description: String, media_class: &'static str) -> Result<u32> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.send(Command::CreateVirtualNode {
            node_name,
            node_description,
            media_class,
            reply: reply_tx,
        })?;
        recv_result(reply_rx)
    }

    pub fn destroy_node(&self, node_handle: u32) -> Result<()> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.send(Command::DestroyNode { node_handle, reply: reply_tx })?;
        recv_result(reply_rx)
    }

    pub fn set_mute(&self, node_id: u32, mute: bool) -> Result<()> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.send(Command::SetMute { node_id, mute, reply: reply_tx })?;
        recv_result(reply_rx)
    }

    fn send(&self, command: Command) -> Result<()> {
        self.command_tx
            .send(command)
            .map_err(|_| BackendError::Message("pipewire thread is not running".to_string()))
    }
}

impl Drop for PipewireBackend {
    fn drop(&mut self) {
        let _ = self.command_tx.send(Command::Shutdown);
    }
}

fn recv_result<T>(reply_rx: mpsc::Receiver<Result<T>>) -> Result<T> {
    reply_rx
        .recv()
        .map_err(|_| BackendError::Message("pipewire thread did not respond".to_string()))?
}

/// Runs on the dedicated PipeWire thread for the lifetime of the backend.
fn run_loop(command_rx: pipewire::channel::Receiver<Command>, event_tx: mpsc::Sender<BackendEvent>) -> Result<()> {
    pipewire::init();

    let mainloop = MainLoopRc::new(None).map_err(|err| BackendError::Message(format!("mainloop init failed: {err}")))?;
    let context = ContextRc::new(&mainloop, None).map_err(|err| BackendError::Message(format!("context init failed: {err}")))?;
    let core = context
        .connect_rc(None)
        .map_err(|err| BackendError::Message(format!("failed to connect to pipewire: {err}")))?;
    let registry = core
        .get_registry_rc()
        .map_err(|err| BackendError::Message(format!("failed to get registry: {err}")))?;

    // Live graph state, rebuilt incrementally from global/global_remove
    // events instead of being re-parsed from pw-dump on every request.
    let state = std::rc::Rc::new(std::cell::RefCell::new(GraphSnapshot::default()));
    // Raw port props keyed by port id, so we can resolve a port's owning
    // node id and channel when a Link's info arrives (link info gives us
    // port ids, not the richer PortRecord directly).
    let port_owner = std::rc::Rc::new(std::cell::RefCell::new(HashMap::<u32, (u32, PortDirection)>::new()));
    // Most-recently-seen GlobalObject per object id, so commands that need
    // a typed proxy later (e.g. SetMute -> bind Node -> set_param) can
    // bind by id instead of needing to hold every proxy alive forever.
    let known_globals = std::rc::Rc::new(
        std::cell::RefCell::new(HashMap::<
            u32,
            std::rc::Rc<pipewire::registry::GlobalObject<pipewire::properties::PropertiesBox>>,
        >::new()),
    );
    // Proxies for objects we created ourselves (virtual sink/mic nodes,
    // links between app ports and the sink). With `object.linger=false`
    // (patchcord's default), a created object only lives as long as its
    // proxy, so these MUST be kept alive on the loop thread until the
    // object is explicitly destroyed — dropping the proxy is what actually
    // removes the object from PipeWire.
    let created_nodes = std::rc::Rc::new(std::cell::RefCell::new(HashMap::<u32, Node>::new()));
    let created_links = std::rc::Rc::new(std::cell::RefCell::new(HashMap::<u32, Link>::new()));

    let state_for_global = state.clone();
    let port_owner_for_global = port_owner.clone();
    let known_globals_for_global = known_globals.clone();
    let event_tx_for_global = event_tx.clone();

    let state_for_remove = state.clone();
    let port_owner_for_remove = port_owner.clone();
    let known_globals_for_remove = known_globals.clone();
    let event_tx_for_remove = event_tx.clone();

    let _global_listener = registry
        .add_listener_local()
        .global(move |global| {
            known_globals_for_global.borrow_mut().insert(global.id, std::rc::Rc::new(global.to_owned()));
            match global.type_ {
                ObjectType::Node => {
                    logger::trace(&format!(
                        "[pipewire] global Node id={} name={:?}",
                        global.id,
                        global.props.as_ref().and_then(|p| p.get("node.name"))
                    ));
                    let props: HashMap<String, String> = global
                        .props
                        .as_ref()
                        .map(|p| p.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect())
                        .unwrap_or_default();

                    let mut record = NodeRecord {
                        id: global.id,
                        props: props
                            .into_iter()
                            .map(|(k, v)| (k, miniserde::json::Value::String(v)))
                            .collect(),
                        ports: Vec::new(),
                    };

                    // Preserve any ports already seen for this node id
                    // (registry event ordering isn't guaranteed to be
                    // nodes-before-ports).
                    if let Some(existing) = state_for_global.borrow().nodes.get(&global.id) {
                        record.ports = existing.ports.clone();
                    }

                    state_for_global.borrow_mut().nodes.insert(global.id, record);
                    let _ = event_tx_for_global.send(BackendEvent::GraphChanged);
                }
                ObjectType::Port => {
                    logger::trace(&format!(
                        "[pipewire] global Port id={} node={:?} dir={:?} channel={:?}",
                        global.id,
                        global.props.as_ref().and_then(|p| p.get("node.id")),
                        global.props.as_ref().and_then(|p| p.get("port.direction")),
                        global.props.as_ref().and_then(|p| p.get("audio.channel")),
                    ));
                    let Some(props) = &global.props else { return };
                    let Some(node_id) = props.get("node.id").and_then(|v| v.parse::<u32>().ok()) else {
                        return;
                    };
                    let Some(direction) = props.get("port.direction").and_then(PortDirection::parse) else {
                        return;
                    };

                    port_owner_for_global.borrow_mut().insert(global.id, (node_id, direction));

                    let port = PortRecord {
                        id: global.id,
                        direction,
                        channel: props.get("audio.channel").map(str::to_string),
                        port_index: props.get("port.id").map(str::to_string),
                        path: Some(global.id.to_string()),
                        port_name: props.get("port.name").map(str::to_string),
                        object_path: props.get("object.path").map(str::to_string),
                    };

                    if let Some(node) = state_for_global.borrow_mut().nodes.get_mut(&node_id) {
                        node.ports.push(port);
                    }
                    let _ = event_tx_for_global.send(BackendEvent::GraphChanged);
                }
                ObjectType::Link => {
                    let Some(props) = &global.props else { return };
                    let parse = |key: &str| props.get(key).and_then(|v| v.parse::<u32>().ok());

                    let (Some(output_node), Some(output_port), Some(input_node), Some(input_port)) =
                        (parse("link.output.node"), parse("link.output.port"), parse("link.input.node"), parse("link.input.port"))
                    else {
                        return;
                    };

                    state_for_global.borrow_mut().links.insert(
                        global.id,
                        LinkRecord {
                            output_node,
                            output_port,
                            input_node,
                            input_port,
                        },
                    );
                    let _ = event_tx_for_global.send(BackendEvent::GraphChanged);
                }
                _ => {}
            }
        })
        .global_remove(move |id| {
            logger::trace(&format!("[pipewire] global_remove id={id}"));
            let mut state = state_for_remove.borrow_mut();
            state.nodes.remove(&id);
            state.links.remove(&id);
            port_owner_for_remove.borrow_mut().remove(&id);
            known_globals_for_remove.borrow_mut().remove(&id);
            let _ = event_tx_for_remove.send(BackendEvent::GraphChanged);
        })
        .register();

    // Live default-sink tracking via PipeWire's `default.audio.sink`
    // metadata key, replacing `pactl get-default-sink` polling entirely:
    // this listener fires immediately whenever the default changes.
    let state_for_metadata = state.clone();
    let event_tx_for_metadata = event_tx.clone();
    let registry_for_metadata = registry.clone();
    let _metadata_listener = registry
        .add_listener_local()
        .global(move |global| {
            if global.type_ != ObjectType::Metadata {
                return;
            }
            let Some(props) = &global.props else { return };
            let name = props.get("metadata.name");
            logger::trace(&format!("[pipewire] saw metadata object id={} name={:?}", global.id, name));
            if name != Some("default") {
                return;
            }

            // Bind to the existing global (not create_object, which would
            // instead ask the server to create a brand new object) to
            // receive property-change events on it.
            let metadata: pipewire::metadata::Metadata = match registry_for_metadata.bind(global) {
                Ok(m) => m,
                Err(_) => return,
            };

            let state = state_for_metadata.clone();
            let event_tx = event_tx_for_metadata.clone();
            // Keep the listener alive for the lifetime of the loop thread
            // by leaking it into a thread-local-ish Rc; this is acceptable
            // since the whole loop thread is torn down together on
            // shutdown.
            let listener = metadata
                .add_listener_local()
                .property(move |_subject, key, _type, value| {
                    if key == Some("default.audio.sink") {
                        let sink_name = value.and_then(|v| {
                            // The value is a small JSON object like
                            // {"name":"alsa_output...."}; extract just the name.
                            miniserde::json::from_str::<DefaultSinkName>(v).ok().map(|d| d.name)
                        });
                        state.borrow_mut().default_sink_name = sink_name.clone();
                        let _ = event_tx.send(BackendEvent::DefaultSinkChanged(sink_name));
                    }
                    0
                })
                .register();
            std::mem::forget(listener);
            std::mem::forget(metadata);
        })
        .register();

    let mainloop_for_commands = mainloop.clone();
    let state_for_commands = state.clone();
    let core_for_commands = core.clone();
    let registry_for_commands = registry.clone();
    let known_globals_for_commands = known_globals.clone();
    let created_nodes_for_commands = created_nodes.clone();
    let created_links_for_commands = created_links.clone();

    let _command_listener = command_rx.attach(mainloop.loop_(), move |command| {
        handle_command(
            &core_for_commands,
            &registry_for_commands,
            &known_globals_for_commands,
            &created_nodes_for_commands,
            &created_links_for_commands,
            &state_for_commands,
            &mainloop_for_commands,
            command,
        );
    });

    mainloop.run();

    Ok(())
}

#[derive(miniserde::Deserialize)]
struct DefaultSinkName {
    name: String,
}

fn handle_command(
    core: &pipewire::core::CoreRc,
    registry: &pipewire::registry::RegistryRc,
    known_globals: &std::rc::Rc<
        std::cell::RefCell<
            HashMap<u32, std::rc::Rc<pipewire::registry::GlobalObject<pipewire::properties::PropertiesBox>>>,
        >,
    >,
    created_nodes: &std::rc::Rc<std::cell::RefCell<HashMap<u32, Node>>>,
    created_links: &std::rc::Rc<std::cell::RefCell<HashMap<u32, Link>>>,
    state: &std::rc::Rc<std::cell::RefCell<GraphSnapshot>>,
    mainloop: &MainLoopRc,
    command: Command,
) {
    match command {
        Command::Snapshot { reply } => {
            let _ = reply.send(state.borrow().clone());
        }
        Command::CreateLink {
            output_node,
            output_port,
            input_node,
            input_port,
            reply,
        } => {
            let result = core
                .create_object::<Link>(
                    "link-factory",
                    &properties! {
                        *keys::LINK_OUTPUT_NODE => output_node.to_string().as_str(),
                        *keys::LINK_OUTPUT_PORT => output_port.to_string().as_str(),
                        *keys::LINK_INPUT_NODE => input_node.to_string().as_str(),
                        *keys::LINK_INPUT_PORT => input_port.to_string().as_str(),
                        *keys::OBJECT_LINGER => "false",
                    },
                )
                .map(|link| {
                    let handle = next_handle();
                    // Keep the proxy alive; dropping it would destroy the
                    // remote object (object.linger=false).
                    created_links.borrow_mut().insert(handle, link);
                    handle
                })
                .map_err(|err| BackendError::Message(format!("failed to create link: {err}")));
            let _ = reply.send(result);
        }
        Command::DestroyLink { link_handle, reply } => {
            // Dropping the (retained) proxy removes the remote object since
            // object.linger=false. If we don't have a proxy for this handle,
            // fall back to a registry-level destroy by id.
            let result = match created_links.borrow_mut().remove(&link_handle) {
                Some(_) => Ok(()),
                None => Err(BackendError::Message(format!("unknown link handle {link_handle}"))),
            };
            let _ = reply.send(result);
        }
        Command::CreateVirtualNode {
            node_name,
            node_description,
            media_class,
            reply,
        } => {
            let result = core
                .create_object::<Node>(
                    "adapter",
                    &properties! {
                        *keys::FACTORY_NAME => "support.null-audio-sink",
                        *keys::NODE_NAME => node_name.as_str(),
                        *keys::NODE_DESCRIPTION => node_description.as_str(),
                        *keys::MEDIA_CLASS => media_class,
                        "audio.position" => "FL,FR",
                    },
                )
                .map(|node| {
                    let handle = next_handle();
                    // Keep the proxy alive; dropping it would destroy the
                    // remote object (object.linger=false).
                    created_nodes.borrow_mut().insert(handle, node);
                    handle
                })
                .map_err(|err| BackendError::Message(format!("failed to create virtual node: {err}")));
            let _ = reply.send(result);
        }
        Command::DestroyNode { node_handle, reply } => {
            let result = match created_nodes.borrow_mut().remove(&node_handle) {
                Some(_) => Ok(()),
                None => Err(BackendError::Message(format!("unknown node handle {node_handle}"))),
            };
            let _ = reply.send(result);
        }
        Command::SetMute { node_id, mute, reply } => {
            let result = (|| {
                // Find the most recent global object for this node id and
                // bind a typed Node proxy to it, so we can call set_param.
                let global = known_globals.borrow().get(&node_id).cloned().ok_or_else(|| {
                    BackendError::Message(format!("no known global for node {node_id}"))
                })?;
                let node = registry
                    .bind::<Node, _>(&*global)
                    .map_err(|err| BackendError::Message(format!("failed to bind node {node_id}: {err}")))?;

                let mut vec = Vec::<u8>::new();
                decode::serialize_mute_props(Cursor::new(&mut vec), mute).map_err(|err| {
                    BackendError::Message(format!("failed to build mute props pod: {err}"))
                })?;
                let pod = libspa::pod::Pod::from_bytes(&vec)
                    .ok_or_else(|| BackendError::Message("failed to parse built props pod".to_string()))?;

                node.set_param(libspa::param::ParamType::Props, 0, pod);

                Ok(())
            })();
            let _ = reply.send(result);
        }
        Command::Shutdown => {
            mainloop.quit();
        }
    }
}

