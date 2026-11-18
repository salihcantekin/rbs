//! macOS helper app entry (renderer, GPU and utility processes). On Linux and
//! Windows the main `rbs-browser-cef` executable plays this role itself.

use cef::{args::Args, *};

#[path = "../engine.rs"]
#[allow(dead_code, reason = "the helper does not read the settings paths")]
mod engine;

fn main() {
    let args = Args::new();
    // The helper loads the same downloaded engine the browser process named in
    // `RBS_CEF_RUNTIME` (inherited), before any CEF call.
    let engine = match engine::load(true) {
        Ok(engine) => engine,
        Err(code) => std::process::exit(code),
    };
    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);
    let code = execute_process(
        Some(args.as_main_args()),
        None::<&mut App>,
        std::ptr::null_mut(),
    );
    drop(engine);
    std::process::exit(code.max(0));
}
