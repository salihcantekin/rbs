//! Prints whether this process may listen to and post input events (TCC preflight;
//! never prompts).

fn main() {
    #[cfg(target_os = "macos")]
    {
        let listen = objc2_core_graphics::CGPreflightListenEventAccess();
        let post = objc2_core_graphics::CGPreflightPostEventAccess();
        println!("listen (Input Monitoring): {listen}, post (Accessibility): {post}");
    }
}
