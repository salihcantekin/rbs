//! Embeds the RBS logo as the Windows executable's icon resource (the file icon and the
//! taskbar icon). Nothing happens on other targets.

fn main() {
    println!("cargo:rerun-if-changed=assets/brand/rbs.ico");
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_windows_icon();
    }
}

#[cfg(windows)]
fn embed_windows_icon() {
    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/brand/rbs.ico");
    if let Err(e) = res.compile() {
        println!("cargo:warning=could not embed the Windows icon: {e}");
    }
}

#[cfg(not(windows))]
fn embed_windows_icon() {}
