//! MeshFlow — multi-agent AI workstation.
//!
//! This binary does one thing: stand up the Tokio runtime, then hand off to the UI. Freya has its
//! own single-threaded runtime and `#[tokio::main]` interferes with its event loop, so the runtime
//! is built by hand and entered for the life of the process.

use mf_engine::proto::{EngineCommand, EngineEvent};
use tokio::sync::{broadcast, mpsc};

/// Room for a burst of token deltas before a slow view starts missing them. A lagging receiver is
/// logged, not fatal.
const EVENT_CAPACITY: usize = 1024;

fn main() -> anyhow::Result<()> {
    let (evt_tx, _) = broadcast::channel::<EngineEvent>(EVENT_CAPACITY);
    // Held to the end of `main`: the file writer flushes on drop, so an early drop would truncate
    // exactly the log lines a crash report needs.
    let _log_guard = mf_engine::logging::init(evt_tx.clone());

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    // This guard must stay alive for the whole program: it is what makes tokio APIs (channels,
    // timers, reqwest) usable from the UI thread.
    let _guard = rt.enter();

    // The entire coupling between the two worlds, in four lines. The engine runs on the Tokio
    // pool; the UI runs on Freya's single-threaded runtime; neither can touch the other's state.
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<EngineCommand>();
    rt.spawn(mf_engine::run(cmd_rx, evt_tx.clone()));

    mf_ui::launch_app(cmd_tx, evt_tx);
    Ok(())
}
