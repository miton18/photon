//! A single shared tokio runtime for all background HTTP work. The GTK main
//! thread never blocks: callers `runtime().spawn(...)` and send results back over
//! an `async-channel` that's polled on the GLib main context.

use std::sync::OnceLock;
use tokio::runtime::Runtime;

pub fn runtime() -> &'static Runtime {
    static RT: OnceLock<Runtime> = OnceLock::new();
    RT.get_or_init(|| Runtime::new().expect("failed to start tokio runtime"))
}
