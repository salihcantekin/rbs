//! `no_hard_coded_visual_values` (rule U6, CLAUDE.md rule 15): reads every `.slint`
//! file of the app and fails on a length, duration or colour written as a literal
//! instead of a `Tokens.*` / `Theme.*` property, so user themes (ADR-0057) reach every
//! part of the UI.
//!
//! Flagged: a number with a length or time unit (`6px`, `0.5px`, `400ms`, `2s`), a
//! `#rrggbb` colour, and the colour functions `rgb(`, `rgba(`, `hsv(` and `Colors.`.
//! Allowed without a note, because they are not design values (the `ui-review` skill,
//! "Allowed literals"): zero (`0px`, `0ms`), a unit conversion (`/ 1px`, `* 1px`) and
//! a "no limit" sentinel of at least 10000 px. Angles and percentages are geometry and
//! are not checked. Anything else needs `// literal: <why>` at the end of its line or on
//! the line above it (or `// literal-begin: <why>` ... `// literal-end` around a block),
//! which a reviewer then judges; colours that are content, such as the colour picker's
//! own fields and preset colours, are the usual case.
//!
//! The reading is lexical: string literals and comments are skipped; the rest is
//! scanned for the patterns above. It does not see values set from Rust.

#![allow(clippy::expect_used, clippy::unwrap_used)] // Tests fail by panicking.

use std::fs;
use std::path::PathBuf;

/// Units that make a number a length or a duration.
const UNITS: [&str; 6] = ["px", "phx", "pt", "rem", "ms", "s"];

/// Colour functions that make a literal colour.
const COLOUR_CALLS: [&str; 4] = ["rgb(", "rgba(", "hsv(", "Colors."];

/// Smallest value read as a "no limit" sentinel (`max-width: 100000px`).
const SENTINEL: f64 = 10_000.0;

/// The marker that allows a literal on its line or the next line.
const MARKER: &str = "// literal:";

/// Markers around a block of lines whose literals are allowed (a list of preset colours).
const BLOCK_BEGIN: &str = "// literal-begin:";
const BLOCK_END: &str = "// literal-end";

/// One line split into the code outside strings and the comment after `//`.
fn split(line: &str) -> (String, &str) {
    let mut code = String::with_capacity(line.len());
    let mut chars = line.char_indices().peekable();
    let mut in_string = false;
    while let Some((i, c)) = chars.next() {
        if in_string {
            if c == '\\' {
                chars.next();
            } else if c == '"' {
                in_string = false;
                code.push('"');
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                code.push('"');
            }
            '/' if matches!(chars.peek(), Some((_, '/'))) => return (code, &line[i..]),
            '/' if matches!(chars.peek(), Some((_, '*'))) => {
                // Inline `/* ... */` (callback parameter names): skip to its end.
                chars.next();
                let mut last = ' ';
                for (_, c) in chars.by_ref() {
                    if last == '*' && c == '/' {
                        break;
                    }
                    last = c;
                }
                code.push(' ');
            }
            c => code.push(c),
        }
    }
    (code, "")
}

/// Whether `c` continues a name, so a digit after it is part of the name (`space-2`).
fn name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')
}

/// The literal values in one line of code (no strings, no comments).
fn literals(code: &str) -> Vec<String> {
    let mut found = Vec::new();
    let bytes: Vec<char> = code.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        let after_name = i > 0 && name_char(bytes[i - 1]);
        if c == '#' && !after_name {
            let hex = bytes[i + 1..]
                .iter()
                .take_while(|c| c.is_ascii_hexdigit())
                .count();
            if matches!(hex, 3 | 4 | 6 | 8) {
                found.push(bytes[i..=i + hex].iter().collect());
            }
            i += 1 + hex;
            continue;
        }
        if c.is_ascii_digit() && !after_name {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == '.') {
                i += 1;
            }
            let unit_start = i;
            while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
                i += 1;
            }
            let number: String = bytes[start..unit_start].iter().collect();
            let unit: String = bytes[unit_start..i].iter().collect();
            if !UNITS.contains(&unit.as_str()) {
                continue;
            }
            let value: f64 = number.parse().unwrap_or(f64::NAN);
            let before = bytes[..start].iter().rev().find(|c| !c.is_whitespace());
            let conversion = value == 1.0 && matches!(before, Some('/' | '*'));
            let sentinel = unit == "px" && value >= SENTINEL;
            if value != 0.0 && !conversion && !sentinel {
                found.push(format!("{number}{unit}"));
            }
            continue;
        }
        if !after_name {
            let rest: String = bytes[i..].iter().take(8).collect();
            if let Some(call) = COLOUR_CALLS.iter().find(|f| rest.starts_with(**f)) {
                found.push((*call).to_string());
                i += call.len();
                continue;
            }
        }
        i += 1;
    }
    found
}

/// Every unmarked literal in one file, as `file:line: value`.
fn check(file: &str, src: &str) -> Vec<String> {
    let mut problems = Vec::new();
    let mut marked_above = false;
    let mut in_block = false;
    for (n, line) in src.lines().enumerate() {
        let (code, comment) = split(line);
        if comment.starts_with(BLOCK_BEGIN) {
            in_block = true;
        } else if comment.starts_with(BLOCK_END) {
            in_block = false;
        }
        let marked = in_block || marked_above || comment.starts_with(MARKER);
        // A marker alone on its line allows the next line.
        marked_above = code.trim().is_empty() && comment.starts_with(MARKER);
        if marked {
            continue;
        }
        for value in literals(&code) {
            problems.push(format!("{file}:{}: {value}", n + 1));
        }
    }
    assert!(!in_block, "{file}: `{BLOCK_BEGIN}` without `{BLOCK_END}`");
    problems
}

#[test]
fn no_hard_coded_visual_values() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("ui");
    let mut files: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "slint"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no .slint files in {}", dir.display());
    let mut problems = Vec::new();
    for path in &files {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        problems.extend(check(&name, &fs::read_to_string(path).unwrap()));
    }
    assert!(
        problems.is_empty(),
        "hard-coded visual values (use a Tokens.*/Theme.* token, add one if none fits, \
         or mark a non-design value with `{MARKER} <why>`; see the ui-review skill):\n{}",
        problems.join("\n")
    );
}

#[test]
fn literals_are_found_and_allowed_ones_are_not() {
    let found = |code: &str| literals(&split(code).0);
    assert_eq!(found("gap: 6px;"), ["6px"]);
    assert_eq!(found("letter-spacing: 0.5px;"), ["0.5px"]);
    assert_eq!(found("interval: 400ms;"), ["400ms"]);
    assert_eq!(found("background: #1b1e23;"), ["#1b1e23"]);
    assert_eq!(found("color: rgb(1, 2, 3);"), ["rgb("]);
    assert_eq!(found("color: hsv(h, 1, 1);"), ["hsv("]);
    assert!(found("border-width: show ? Tokens.size-border : 0px;").is_empty());
    assert!(found("round(self.width / 1px)").is_empty());
    assert!(found("max-width: open ? Tokens.size-menu-sm : 100000px;").is_empty());
    assert!(found("padding: Tokens.space-2; height: 2 * Tokens.space-8;").is_empty());
    assert!(found("transform-rotation: v ? 90deg : 0deg; width: 50%;").is_empty());
    assert!(found("text: @tr(\"6px #fff\"); // 12px in a comment").is_empty());
    assert!(found("callback drag(int /* 2px */, length);").is_empty());
    assert!(found("hue: self.value.to-hsv().hue;").is_empty());
}

#[test]
fn a_marker_allows_its_line_and_the_next() {
    let src = "a: 6px; // literal: mirrors HANDLE_THICKNESS\n\
               // literal: picker content\n\
               b: #ffffff;\n\
               c: #000000;\n\
               // literal-begin: presets\n\
               d: #111111;\n\
               // literal-end\n\
               e: 2px;\n";
    assert_eq!(
        check("x.slint", src),
        ["x.slint:4: #000000", "x.slint:8: 2px"]
    );
}
