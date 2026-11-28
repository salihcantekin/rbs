//! Compiles the vendored libopus (float build, no SIMD dispatch) with the `cc` crate.
//!
//! No CMake, autotools or system library is involved (ADR-0046's build goal, ADR-0139
//! decision 3). The source set is every `.c` file below `vendor/libopus`; the vendored
//! tree holds only the files of the reference float build (see `vendor/README.md`).

use std::fs;
use std::path::{Path, PathBuf};

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, out);
        } else if path.extension().is_some_and(|e| e == "c") {
            out.push(path);
        }
    }
}

fn main() {
    let root = PathBuf::from("vendor/libopus");
    let mut sources = Vec::new();
    collect(&root, &mut sources);
    sources.sort();
    println!("cargo:rerun-if-changed=vendor");
    println!("cargo:rerun-if-changed=build.rs");

    let mut build = cc::Build::new();
    build
        .files(&sources)
        .include(root.join("include"))
        .include(root.join("celt"))
        .include(root.join("silk"))
        .include(root.join("silk/float"))
        .include(root.join("src"))
        .define("OPUS_BUILD", None)
        .define("USE_ALLOCA", None)
        .define("PACKAGE_VERSION", "\"1.5.2\"")
        .opt_level(2)
        .warnings(false);
    // MSVC has no C99 `lrint`; libopus then rounds with a cast.
    if !build.get_compiler().is_like_msvc() {
        build.define("HAVE_LRINTF", None).define("HAVE_LRINT", None);
    }
    build.compile("rbs_opus");
}
