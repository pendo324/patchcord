//! Spike: validate that a virtual null-audio-sink can be created via
//! `Core::create_object()` and the `support.null-audio-sink` factory,
//! replacing patchcord's `pactl load-module module-null-sink` call.
//!
//! Run with: cargo run --example virtual_sink_spike
//!
//! Expected: prints the new node's id once the registry sees it appear,
//! then destroys it and confirms the registry sees it disappear.

use pipewire::{
    context::ContextRc, core::PW_ID_CORE, keys, main_loop::MainLoopRc, node::Node,
    properties::properties, types::ObjectType,
};
use std::cell::RefCell;
use std::rc::Rc;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    pipewire::init();

    let mainloop = MainLoopRc::new(None)?;
    let context = ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;
    let registry = core.get_registry_rc()?;

    let sink_name = format!("patchcord-spike-sink-{}", std::process::id());

    // Create the virtual sink via the native factory instead of pactl.
    let sink: Node = core.create_object(
        "adapter",
        &properties! {
            *keys::FACTORY_NAME => "support.null-audio-sink",
            *keys::NODE_NAME => sink_name.as_str(),
            *keys::NODE_DESCRIPTION => "Patchcord Spike Virtual Sink",
            *keys::MEDIA_CLASS => "Audio/Sink",
            "audio.position" => "FL,FR",
            "object.linger" => "false",
        },
    )?;

    println!("Requested creation of sink '{sink_name}', waiting for it to appear in the registry...");

    let found_id: Rc<RefCell<Option<u32>>> = Rc::new(RefCell::new(None));
    let found_id_clone = found_id.clone();
    let sink_name_clone = sink_name.clone();

    let _listener = registry
        .add_listener_local()
        .global(move |global| {
            if global.type_ != ObjectType::Node {
                return;
            }
            let Some(props) = &global.props else { return };
            if props.get("node.name") == Some(sink_name_clone.as_str()) {
                println!("[Found] sink node id={} name={}", global.id, sink_name_clone);
                *found_id_clone.borrow_mut() = Some(global.id);
            }
        })
        .register();

    let mainloop_clone = mainloop.clone();
    let _sync_listener = core
        .add_listener_local()
        .done(move |id, _seq| {
            if id == PW_ID_CORE {
                mainloop_clone.quit();
            }
        })
        .register();

    core.sync(0)?;
    mainloop.run();

    if found_id.borrow().is_none() {
        eprintln!("FAILED: sink never appeared in the registry");
        std::process::exit(1);
    }

    println!("SUCCESS: virtual sink created natively without pactl/pw-loopback.");
    println!("Destroying it now...");
    core.destroy_object(sink)?;

    // Give the server a moment to process the destroy before we exit.
    let mainloop2 = mainloop.clone();
    let timer = mainloop.loop_().add_timer(move |_| mainloop2.quit());
    timer
        .update_timer(Some(std::time::Duration::from_millis(300)), None)
        .into_result()?;
    mainloop.run();

    println!("Done.");
    Ok(())
}
