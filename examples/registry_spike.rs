//! Spike: validate that the `pipewire` crate can replace patchcord's
//! `pw-dump`-based node listing with a live registry listener.
//!
//! Run with: cargo run --example registry_spike
//!
//! Expected output: a line per PipeWire global object currently known to
//! the server (id, type, and a few interesting node properties), followed
//! by "--- done with initial burst ---" once `Core::sync` resolves.

use pipewire::{context::ContextRc, main_loop::MainLoopRc, types::ObjectType};
use std::cell::RefCell;
use std::rc::Rc;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    pipewire::init();

    let mainloop = MainLoopRc::new(None)?;
    let context = ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;
    let registry = core.get_registry_rc()?;

    let seen_count = Rc::new(RefCell::new(0u32));
    let seen_count_clone = seen_count.clone();

    let _listener = registry
        .add_listener_local()
        .global(move |global| {
            *seen_count_clone.borrow_mut() += 1;

            let name = global
                .props
                .as_ref()
                .and_then(|p| p.get("node.name").or_else(|| p.get("port.name")))
                .unwrap_or("<no name>");

            let media_class = global
                .props
                .as_ref()
                .and_then(|p| p.get("media.class"))
                .unwrap_or("");

            match global.type_ {
                ObjectType::Node => {
                    println!(
                        "[Node]     id={:<5} name={:<40} media.class={}",
                        global.id, name, media_class
                    );
                }
                ObjectType::Port => {
                    // Ports are numerous and noisy; skip printing them individually
                    // for this spike, we only care that Node/Link/Metadata work.
                }
                ObjectType::Link => {
                    println!("[Link]     id={}", global.id);
                }
                ObjectType::Metadata => {
                    println!("[Metadata] id={:<5} name={}", global.id, name);
                }
                _ => {}
            }
        })
        .register();

    let mainloop_clone = mainloop.clone();
    let seen_count_clone2 = seen_count.clone();
    let _sync_listener = core
        .add_listener_local()
        .done(move |id, _seq| {
            if id == pipewire::core::PW_ID_CORE {
                println!(
                    "--- done with initial burst, saw {} globals ---",
                    seen_count_clone2.borrow()
                );
                mainloop_clone.quit();
            }
        })
        .register();

    core.sync(0)?;

    mainloop.run();

    Ok(())
}
