//! The MSVC linker warns (LNK4104) that `DllGetClassObject` and `DllCanUnloadNow`
//! should be `PRIVATE` exports, that is, left out of the DLL's import library. Nothing
//! links against that import library (COM loads the DLL by path), so the warning is
//! silenced rather than adding a module-definition file next to Rust's own exports.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let windows = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows");
    let msvc = std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
    if windows && msvc {
        println!("cargo:rustc-cdylib-link-arg=/IGNORE:4104");
    }
}
