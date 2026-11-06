//! Every callback the shell hands to Slint runs under the panic boundary (ADR-0054
//! decision 2). `clippy.toml` covers the Slint functions; this covers the generated
//! `on_*` callbacks: each is installed through the `on!` macro (whose body goes through
//! the guarded `Ctx::update`) or starts with `entry::guard`. Reads the sources only.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::path::Path;

#[test]
fn every_generated_callback_is_guarded() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut unguarded = Vec::new();
    let mut seen = 0;
    for entry in std::fs::read_dir(&src).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|e| e != "rs") || path.ends_with("entry.rs") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            let Some(at) = line.find(".on_") else {
                continue;
            };
            let call = &line[at..];
            if !call.contains("(move ||") && !call.contains("(|") {
                continue;
            }
            seen += 1;
            let next = lines.get(i + 1).map_or("", |l| l.trim_start());
            if !call.contains("entry::guard") && !next.starts_with("entry::guard") {
                unguarded.push(format!("{}:{}: {}", path.display(), i + 1, line.trim()));
            }
        }
    }
    assert!(
        seen > 0,
        "the scan found no callbacks: has the shell moved?"
    );
    assert!(
        unguarded.is_empty(),
        "callbacks without entry::guard:\n{}",
        unguarded.join("\n")
    );
}
