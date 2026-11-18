//! How the runner finds the downloaded engine (add-ons spec "Browser engine consumer").
//!
//! - Linux: release builds have no `rpath`; the studio sets `LD_LIBRARY_PATH` to the
//!   runtime's library directory. Debug builds keep `rpath=$ORIGIN` for the engine
//!   that `cef-dll-sys` copies next to the executable.
//! - Windows: `libcef.dll` is delay-loaded (`delayimp` comes from `cef-dll-sys`), so the
//!   runner loads it from the runtime before the first CEF call.

fn main() {
    println!("cargo:rerun-if-env-changed=PROFILE");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let release = std::env::var("PROFILE").as_deref() == Ok("release");
    if os == "linux" && !release {
        println!("cargo:rustc-link-arg-bins=-Wl,-rpath,$ORIGIN");
    }
    if os == "windows" {
        println!("cargo:rustc-link-arg-bins=/DELAYLOAD:libcef.dll");
    }
}
