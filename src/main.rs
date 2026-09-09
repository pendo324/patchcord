mod logger;
mod patchbay;

use std::io::{self, BufRead, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use miniserde::{Deserialize, Serialize};
use patchbay::{AudioSharePatchbay, PatchbayConfig, has_pipewire};

#[derive(Debug, Deserialize)]
struct RequestEnvelope {
	#[serde(default)]
	id: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "method")]
enum Request {
	#[serde(rename = "hasPipeWire")]
	HasPipeWire { id: u64 },

	#[serde(rename = "listShareableNodes")]
	ListShareableNodes {
		id: u64,
		#[serde(default, rename = "includeDevices")]
		include_devices: bool,
	},

	#[serde(rename = "ensureVirtualSink")]
	EnsureVirtualSink { id: u64 },

	#[serde(rename = "routeNodes")]
	RouteNodes {
		id: u64,
		#[serde(rename = "nodeIds")]
		node_ids: Vec<u32>,

		#[serde(default, rename = "onlySpeakers")]
		only_speakers: bool,

		#[serde(default, rename = "onlyDefaultSpeakers")]
		only_default_speakers: bool,

		#[serde(default, rename = "ignoreDevices")]
		ignore_devices: bool,

		#[serde(default, rename = "ignoreVirtual")]
		ignore_virtual: bool,

		#[serde(default, rename = "ignoreInputMedia")]
		ignore_input_media: bool,
	},

	#[serde(rename = "clearRoutes")]
	ClearRoutes { id: u64 },

	#[serde(rename = "setVirtualMicMute")]
	SetVirtualMicMute { id: u64, mute: bool },

	#[serde(rename = "dispose")]
	Dispose { id: u64 },
}

#[derive(Serialize)]
struct SuccessResponse<T> {
	id: u64,
	result: T,
}

#[derive(Serialize)]
struct ErrorResponse {
	id: u64,
	error: String,
}

#[derive(Serialize)]
struct EventMessage {
	event: &'static str,
}

enum IncomingMessage {
	Request(String),
	GraphChanged,
	MonitorDied,
	StdinClosed,
}

fn write_json_line<T: Serialize>(out: &mut impl Write, value: &T) -> io::Result<()> {
	let serialized = miniserde::json::to_string(value);
	out.write_all(serialized.as_bytes())?;
	out.write_all(b"\n")?;
	out.flush()
}

fn write_result<T: Serialize, E: ToString>(out: &mut impl Write, id: u64, result: Result<T, E>) -> io::Result<()> {
	match result {
		Ok(value) => write_json_line(out, &SuccessResponse { id, result: value }),
		Err(err) => write_json_line(
			out,
			&ErrorResponse {
				id,
				error: err.to_string(),
			},
		),
	}
}

/// Dispatches JSON requests to the Patchbay logic.
/// Returns Ok(false) if a Dispose request was processed, indicating the loop should exit.
fn handle_request(out: &mut impl Write, patchbay: &mut AudioSharePatchbay, request: Request) -> io::Result<bool> {
	match request {
		Request::HasPipeWire { id } => {
			write_json_line(
				out,
				&SuccessResponse {
					id,
					result: has_pipewire(),
				},
			)?;
		}
		Request::ListShareableNodes { id, include_devices } => {
			write_result(out, id, patchbay.list_shareable_nodes(include_devices))?;
		}
		Request::EnsureVirtualSink { id } => {
			write_result(out, id, patchbay.ensure_virtual_sink())?;
		}
		Request::RouteNodes {
			id,
			node_ids,
			only_speakers,
			only_default_speakers,
			ignore_devices,
			ignore_virtual,
			ignore_input_media,
		} => {
			let filter = patchbay::RouteFilter {
				only_speakers,
				only_default_speakers,
				ignore_devices,
				ignore_virtual,
				ignore_input_media,
			};
			write_result(out, id, patchbay.route_nodes(node_ids, filter))?;
		}
		Request::ClearRoutes { id } => {
			write_result(out, id, patchbay.clear_routes())?;
		}
		Request::SetVirtualMicMute { id, mute } => {
			write_result(out, id, patchbay.set_virtual_mic_mute(mute))?;
		}
		Request::Dispose { id } => {
			write_result(out, id, patchbay.dispose())?;
			return Ok(false);
		}
	}

	Ok(true)
}

fn parse_args() -> PatchbayConfig {
	let mut config = PatchbayConfig::default();
	let mut args = std::env::args().skip(1);

	while let Some(arg) = args.next() {
		match arg.as_str() {
			"--sink-prefix" => {
				if let Some(val) = args.next() {
					config.sink_prefix = val;
				}
			}
			"--sink-description" => {
				if let Some(val) = args.next() {
					config.sink_description = val;
				}
			}
			"--virtual-mic" => {
				config.virtual_mic = true;
			}
			"--virtual-mic-name" => {
				if let Some(val) = args.next() {
					config.virtual_mic_name = Some(val);
				}
			}
			"--virtual-mic-description" => {
				if let Some(val) = args.next() {
					config.virtual_mic_description = Some(val);
				}
			}
			"-h" | "--help" => {
				println!("Usage: audio-share-helper [OPTIONS]");
				std::process::exit(0);
			}
			_ => {
				logger::warn(&format!("[helper] unknown argument ignored: {arg}"));
			}
		}
	}

	config
}

/// Spawns the thread responsible for reading JSON-RPC lines from Node.js via Stdin.
fn spawn_stdin_thread(tx: mpsc::Sender<IncomingMessage>) {
	thread::spawn(move || {
		let stdin = io::stdin();
		for line in stdin.lock().lines() {
			match line {
				Ok(text) => {
					if text.trim().is_empty() {
						continue;
					}
					if tx.send(IncomingMessage::Request(text)).is_err() {
						return;
					}
				}
				Err(_) => break,
			}
		}
		let _ = tx.send(IncomingMessage::StdinClosed);
	});
}

/// Forwards graph-change events from the native backend into the main
/// protocol loop as `graphChanged` messages, replacing the `pw-mon`
/// subprocess used by the legacy backend.
fn spawn_native_event_thread_concrete(
	events: mpsc::Receiver<patchbay::pw_backend::BackendEvent>,
	tx: mpsc::Sender<IncomingMessage>,
) {
	thread::spawn(move || loop {
		match events.recv() {
			Ok(patchbay::pw_backend::BackendEvent::GraphChanged) => {
				if tx.send(IncomingMessage::GraphChanged).is_err() {
					return;
				}
			}
			Ok(patchbay::pw_backend::BackendEvent::DefaultSinkChanged(_)) => {
				// Not currently surfaced in the protocol output; reserved
				// for the only-default-speakers filter work.
			}
			Err(_) => return,
		}
	});
}

/// Spawns `pw-mon` and a monitoring thread.
fn spawn_pw_mon_thread(tx: mpsc::Sender<IncomingMessage>) -> Option<Child> {
	let mut child = Command::new("pw-mon")
		.env("LC_ALL", "C")
		.env("LANG", "C")
		.stdout(Stdio::piped())
		.stderr(Stdio::null())
		.spawn()
		.ok()?;

	let stdout = child.stdout.take()?;

	thread::spawn(move || {
		let reader = io::BufReader::new(stdout);
		let mut last_trigger = Instant::now() - Duration::from_secs(1);

		for line in reader.lines() {
			let Ok(text) = line else { break };

			if text.contains("PipeWire:Interface:Node") || text.contains("PipeWire:Interface:Port") {
				if last_trigger.elapsed() > Duration::from_millis(400) {
					if tx.send(IncomingMessage::GraphChanged).is_err() {
						return;
					}
					last_trigger = Instant::now();
				}
			}
		}
		let _ = tx.send(IncomingMessage::MonitorDied);
	});

	Some(child)
}

fn main() -> io::Result<()> {
	if std::env::args().any(|a| a == "--pw-backend-spike") {
		return run_pw_backend_spike();
	}

	let legacy_backend = std::env::args().any(|a| a == "--legacy-backend");

	let config = parse_args();
	let mut patchbay = AudioSharePatchbay::new(&config, legacy_backend);
	let mut stdout = io::BufWriter::new(io::stdout().lock());

	let (tx, rx) = mpsc::channel();

	spawn_stdin_thread(tx.clone());

	// The native backend reports graph changes through its own event
	// channel (registry global/global_remove listeners); the legacy
	// backend uses an external `pw-mon` subprocess instead.
	let monitor_child = if let Some(native_events) = patchbay.take_native_events() {
		spawn_native_event_thread_concrete(native_events, tx.clone());
		None
	} else {
		spawn_pw_mon_thread(tx)
	};

	// Main loop: Listen for requests from Node.js or events from PipeWire
	for msg in rx {
		match msg {
			IncomingMessage::Request(line) => {
				let request_id = miniserde::json::from_str::<RequestEnvelope>(&line)
					.ok()
					.and_then(|envelope| envelope.id)
					.unwrap_or(0);

				let request = match miniserde::json::from_str::<Request>(&line) {
					Ok(request) => request,
					Err(err) => {
						logger::warn(&format!("[helper] invalid request: {err:?}"));
						write_json_line(
							&mut stdout,
							&ErrorResponse {
								id: request_id,
								error: format!("invalid request: {err:?}"),
							},
						)?;
						continue;
					}
				};

				if !handle_request(&mut stdout, &mut patchbay, request)? {
					break;
				}
			}
			IncomingMessage::GraphChanged => {
				write_json_line(&mut stdout, &EventMessage { event: "graphChanged" })?;
			}
			IncomingMessage::MonitorDied => {
				write_json_line(&mut stdout, &EventMessage { event: "monitorDied" })?;
			}
			IncomingMessage::StdinClosed => {
				logger::info("[helper] stdin closed, exiting...");
				break;
			}
		}
	}

	if let Some(mut child) = monitor_child {
		let _ = child.kill();
		let _ = child.wait();
	}

	let _ = patchbay.dispose();

	Ok(())
}

/// Exercises the native `pw_backend` module end-to-end against a live
/// PipeWire server: connect, snapshot the graph, create a virtual sink,
/// snapshot again to see it appear, route a real node to it, then tear
/// everything down. Not part of the real protocol; a manual validation
/// aid for the pipewire-rs backend rewrite.
fn run_pw_backend_spike() -> io::Result<()> {
	use patchbay::pw_backend::{BackendEvent, PipewireBackend};
	use std::sync::mpsc;
	use std::time::Duration;

	let (event_tx, event_rx) = mpsc::channel();

	println!("Spawning pipewire backend thread...");
	let backend = PipewireBackend::spawn(event_tx).expect("failed to spawn pipewire backend");

	// Give the registry a moment to receive its initial burst of globals.
	thread::sleep(Duration::from_millis(300));
	while event_rx.try_recv().is_ok() {}

	let snapshot = backend.snapshot().expect("snapshot failed");
	println!("Initial snapshot: {} nodes, {} links, default_sink={:?}", snapshot.nodes.len(), snapshot.links.len(), snapshot.default_sink_name);
	let with_ports = snapshot.nodes.values().filter(|n| n.output_ports().next().is_some()).count();
	let app_like = snapshot
		.nodes
		.values()
		.filter(|n| {
			n.prop_str("media.class") == Some("Stream/Output/Audio")
				&& (n.prop_str("application.name").is_some() || n.prop_str("application.process.binary").is_some())
		})
		.collect::<Vec<_>>();
	println!("  nodes with output ports: {with_ports}, app-like stream nodes: {}", app_like.len());
	for n in app_like.iter().take(4) {
		println!(
			"    node {} name={:?} app={:?} outputs={}",
			n.id,
			n.prop_str("node.name"),
			n.prop_str("application.name"),
			n.output_ports().count()
		);
	}

	assert!(snapshot.nodes.len() > 0, "expected at least one node in the live graph");

	println!("Creating a virtual sink via the native backend...");
	let sink_name = format!("pw-backend-spike-sink-{}", std::process::id());
	match backend.create_virtual_node(sink_name.clone(), "PW Backend Spike Sink".to_string(), "Audio/Sink") {
		Ok(_) => println!("create_virtual_node command accepted"),
		Err(err) => println!("create_virtual_node failed: {err}"),
	}

	// Wait for the registry to observe the new node and report it via
	// GraphChanged, then confirm it shows up in a fresh snapshot.
	let mut saw_graph_changed = false;
	let deadline = std::time::Instant::now() + Duration::from_secs(2);
	while std::time::Instant::now() < deadline {
		if let Ok(BackendEvent::GraphChanged) = event_rx.recv_timeout(Duration::from_millis(100)) {
			saw_graph_changed = true;
			break;
		}
	}
	println!("Observed GraphChanged event: {saw_graph_changed}");

	let snapshot2 = backend.snapshot().expect("snapshot failed");
	println!("  post-create snapshot: {} nodes, {} links", snapshot2.nodes.len(), snapshot2.links.len());
	let interesting: Vec<String> = snapshot2
		.nodes
		.values()
		.filter_map(|n| n.prop_str("node.name").map(str::to_string))
		.filter(|n| n.contains("patchcord") || n.contains("spike") || n.contains("sink"))
		.collect();
	println!("  nodes matching spike/sink: {interesting:?}");
	let found = snapshot2
		.nodes
		.values()
		.find(|n| n.prop_str("node.name") == Some(sink_name.as_str()))
		.map(|n| n.id);
	let sink_id = match found {
		Some(id) => {
			println!("SUCCESS: found virtual sink in snapshot, id={id}");
			id
		}
		None => {
			println!("FAILED: virtual sink not found in post-create snapshot");
			return Ok(());
		}
	};

	// Exercise SetMute: mute the sink, confirm the prop flips, unmute.
	println!("Muting sink (SetMute=true)...");
	match backend.set_mute(sink_id, true) {
		Ok(()) => println!("set_mute(true) ok"),
		Err(err) => println!("set_mute(true) failed: {err}"),
	}
	thread::sleep(Duration::from_millis(150));
	if let Some(node) = backend.snapshot().ok().and_then(|s| s.nodes.get(&sink_id).cloned()) {
		println!("  after mute: node.props[\"mute\"]={:?}", node.props.get("mute"));
	}

	println!("Unmuting sink (SetMute=false)...");
	match backend.set_mute(sink_id, false) {
		Ok(()) => println!("set_mute(false) ok"),
		Err(err) => println!("set_mute(false) failed: {err}"),
	}
	thread::sleep(Duration::from_millis(150));

	// Re-mute and hold for external verification (e.g. `pactl list sinks`).
	println!("Re-muting sink and holding 4s for external verification...");
	let _ = backend.set_mute(sink_id, true);
	thread::sleep(Duration::from_secs(4));
	println!("Done holding.");
	if let Some(node) = backend.snapshot().ok().and_then(|s| s.nodes.get(&sink_id).cloned()) {
		println!("  after unmute: node.props[\"mute\"]={:?}", node.props.get("mute"));
	}

	// Exercise link creation: route one output port of a Stream/Output/Audio
	// app node to the sink's first input port, confirm it lands in the
	// live graph, then destroy it.
	let snapshot3 = backend.snapshot().expect("snapshot failed");
	let app = snapshot3
		.nodes
		.values()
		.find(|n| n.prop_str("media.class") == Some("Stream/Output/Audio") && n.output_ports().any(|p| p.path.is_some()))
		.cloned();
	let sink = snapshot3.nodes.get(&sink_id).cloned();
	match (app, sink) {
		(Some(app), Some(sink)) if !app.output_ports().next().is_none() && !sink.input_ports().next().is_none() => {
			let out_port = app.output_ports().next().expect("app has output port").id;
			let in_port = sink.input_ports().next().expect("sink has input port").id;
			println!(
				"Linking app node {} port {out_port} -> sink {} port {in_port}...",
				app.id, sink.id
			);
			match backend.create_link(app.id, out_port, sink.id, in_port) {
				Ok(link_id) => {
					println!("  link created, id={link_id}");
					thread::sleep(Duration::from_millis(250));
					let linked = backend.snapshot().ok().is_some_and(|s| {
						s.links
							.values()
							.any(|l| l.output_node == app.id && l.input_node == sink_id)
					});
					println!("  link visible in graph: {linked}");
					let _ = backend.destroy_link(link_id);
					thread::sleep(Duration::from_millis(200));
					let gone = !backend.snapshot().ok().is_some_and(|s| {
						s.links
							.values()
							.any(|l| l.output_node == app.id && l.input_node == sink_id)
					});
					println!("  link gone after destroy: {gone}");
				}
				Err(err) => println!("  create_link failed: {err}"),
			}
		}
		_ => println!("  SKIP: no suitable app/sink node for link test"),
	}

	// Exercise destroy: remove the sink and confirm it disappears.
	println!("Destroying sink...");
	match backend.destroy_node(sink_id) {
		Ok(()) => println!("destroy_node ok"),
		Err(err) => println!("destroy_node failed: {err}"),
	}
	thread::sleep(Duration::from_millis(150));
	let still_there = backend
		.snapshot()
		.ok()
		.is_some_and(|s| s.nodes.contains_key(&sink_id));
	println!("  sink still present after destroy: {still_there}");

	println!("Spike complete, shutting down backend.");
	drop(backend);
	thread::sleep(Duration::from_millis(200));

	Ok(())
}

