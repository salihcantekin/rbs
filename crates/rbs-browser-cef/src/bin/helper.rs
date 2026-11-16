//! macOS helper app entry (renderer, GPU and utility processes). On Linux and
//! Windows the main `rbs-browser-cef` executable plays this role itself.

use cef::{args::Args, *};

fn main() {
    let args = Args::new();
    #[cfg(target_os = "macos")]
    let _loader = {
        let Ok(exe) = std::env::current_exe() else {
            std::process::exit(2);
        };
        let loader = library_loader::LibraryLoader::new(&exe, true);
        if !loader.load() {
            std::process::exit(3);
        }
        loader
    };
    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);
    let code = execute_process(
        Some(args.as_main_args()),
        None::<&mut App>,
        std::ptr::null_mut(),
    );
    std::process::exit(code.max(0));
}
