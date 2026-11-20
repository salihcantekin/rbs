//! The size ratchet (`cargo xtask check-size`, quality plan WP-03, ADR-0144).
//!
//! Counts the lines of every Rust file under `crates/` and `xtask/` (directories named
//! `target`, `sys` and `vendor` left out) and of every function in them. A file over
//! [`FILE_LIMIT`] lines or a function over [`FN_LIMIT`] lines may not grow past its
//! ceiling in `docs/quality/size-baseline.toml`; one that is not listed there may not
//! pass the limit at all. An increase the owner approved in writing is recorded in
//! `docs/quality/size-exceptions.toml` with `--except`. `--lower` writes smaller
//! numbers down after a split. Neither file is edited by hand.
//!
//! Function lengths are measured from the `fn` line to the line where its braces close,
//! counted on the text with comments, strings and character literals left out. That is
//! a heuristic, but the ceilings are written with the same one, so it is consistent.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use toml::Value;

/// A file longer than this is listed with a ceiling.
pub const FILE_LIMIT: u32 = 2000;
/// A function longer than this is listed with a ceiling.
pub const FN_LIMIT: u32 = 200;

const BASELINE: &str = "docs/quality/size-baseline.toml";
const EXCEPTIONS: &str = "docs/quality/size-exceptions.toml";
const SKIPPED_DIRS: &[&str] = &["target", "sys", "vendor", ".git"];

pub const USAGE: &str = "usage: cargo xtask check-size
       cargo xtask check-size --lower
       cargo xtask check-size --except <file or file::fn> <lines> <reason> <owner's approval> <date>";

/// The measured sizes: files by path, functions by `path::name` (the longest of the
/// functions with that name in the file).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sizes {
    pub files: BTreeMap<String, u32>,
    pub functions: BTreeMap<String, u32>,
}

/// One approved ceiling above the baseline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Exception {
    pub item: String,
    pub lines: u32,
    pub reason: String,
    pub approved: String,
    pub date: String,
}

/// Runs `cargo xtask check-size` with the arguments after the command name.
pub fn run(root: &Path, args: &[String]) -> Result<(), Vec<String>> {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        [] => check(root),
        ["--lower"] => lower(root),
        ["--seed"] => seed(root),
        ["--except", item, lines, reason, approved, date] => {
            let lines = lines
                .parse::<u32>()
                .map_err(|_| vec![format!("lines must be a whole number, got {lines}")])?;
            except(
                root,
                Exception {
                    item: (*item).to_owned(),
                    lines,
                    reason: (*reason).to_owned(),
                    approved: (*approved).to_owned(),
                    date: (*date).to_owned(),
                },
            )
        }
        _ => Err(vec![USAGE.to_owned()]),
    }
}

/// Fails when an item is over its allowed size, or under a listed ceiling (so the
/// lower number is written down and the room cannot be used again).
pub fn check(root: &Path) -> Result<(), Vec<String>> {
    let baseline = read_baseline(&read(root, BASELINE)?)?;
    let exceptions = read_exceptions(&read(root, EXCEPTIONS)?)?;
    let found = measure(root)?;
    let errors = compare(&found, &baseline, &exceptions);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// The errors of `found` against the ceilings.
pub fn compare(found: &Sizes, baseline: &Sizes, exceptions: &[Exception]) -> Vec<String> {
    let mut errors = Vec::new();
    for (kind, limit, found, ceilings) in [
        ("file", FILE_LIMIT, &found.files, &baseline.files),
        ("function", FN_LIMIT, &found.functions, &baseline.functions),
    ] {
        for (item, &lines) in found {
            let allowed = allowed(item, limit, ceilings, exceptions);
            if lines > allowed {
                errors.push(format!(
                    "{kind} {item}: {lines} lines, {allowed} allowed. Split it, or stop and ask \
                     the owner; an approved increase is recorded with `cargo xtask check-size \
                     --except`."
                ));
            } else if lines < allowed && allowed > limit {
                errors.push(format!(
                    "{kind} {item}: {lines} lines, {allowed} allowed. Write the lower number \
                     down with `cargo xtask check-size --lower`."
                ));
            }
        }
        for item in ceilings.keys() {
            if !found.contains_key(item) {
                errors.push(format!(
                    "{kind} {item} is listed but gone. Run `cargo xtask check-size --lower`."
                ));
            }
        }
    }
    errors
}

fn allowed(item: &str, limit: u32, ceilings: &BTreeMap<String, u32>, ex: &[Exception]) -> u32 {
    let base = ceilings.get(item).copied().unwrap_or(limit).max(limit);
    ex.iter()
        .filter(|e| e.item == item)
        .map(|e| e.lines)
        .fold(base, u32::max)
}

fn lower(root: &Path) -> Result<(), Vec<String>> {
    let baseline = read_baseline(&read(root, BASELINE)?)?;
    let mut exceptions = read_exceptions(&read(root, EXCEPTIONS)?)?;
    let found = measure(root)?;
    let mut errors = Vec::new();
    let mut next = Sizes::default();
    for (limit, found, ceilings, out) in [
        (FILE_LIMIT, &found.files, &baseline.files, &mut next.files),
        (
            FN_LIMIT,
            &found.functions,
            &baseline.functions,
            &mut next.functions,
        ),
    ] {
        for (item, &lines) in found {
            let allowed = allowed(item, limit, ceilings, &exceptions);
            if lines > allowed {
                errors.push(format!(
                    "{item}: {lines} lines, {allowed} allowed. --lower never raises a number."
                ));
                if let Some(&ceiling) = ceilings.get(item) {
                    out.insert(item.clone(), ceiling);
                }
                continue;
            }
            if lines > limit {
                let ceiling = ceilings.get(item).copied().unwrap_or(lines).min(lines);
                out.insert(item.clone(), ceiling);
            }
        }
    }
    // An exception is kept only while the item still needs more than its baseline.
    exceptions.retain(|e| {
        let (sizes, base) = if found.files.contains_key(&e.item) {
            (&found.files, next.files.get(&e.item))
        } else {
            (&found.functions, next.functions.get(&e.item))
        };
        sizes
            .get(&e.item)
            .is_some_and(|&lines| base.is_none_or(|&b| lines > b))
    });
    for e in &mut exceptions {
        let lines = found
            .files
            .get(&e.item)
            .or_else(|| found.functions.get(&e.item))
            .copied()
            .unwrap_or(0);
        e.lines = e.lines.min(lines);
    }
    write(root, BASELINE, &write_baseline(&next))?;
    write(root, EXCEPTIONS, &write_exceptions(&exceptions))?;
    if errors.is_empty() {
        println!("check-size: ceilings written");
        Ok(())
    } else {
        Err(errors)
    }
}

/// Writes the first baseline from the tree. Refused once the baseline exists.
fn seed(root: &Path) -> Result<(), Vec<String>> {
    if root.join(BASELINE).exists() {
        return Err(vec![format!("{BASELINE} exists; use --lower or --except")]);
    }
    let found = measure(root)?;
    write(root, BASELINE, &write_baseline(&found))?;
    if !root.join(EXCEPTIONS).exists() {
        write(root, EXCEPTIONS, &write_exceptions(&[]))?;
    }
    println!(
        "check-size: {} files and {} functions written",
        found.files.len(),
        found.functions.len()
    );
    Ok(())
}

fn except(root: &Path, entry: Exception) -> Result<(), Vec<String>> {
    for (name, value) in [
        ("item", &entry.item),
        ("reason", &entry.reason),
        ("owner's approval", &entry.approved),
        ("date", &entry.date),
    ] {
        if value.trim().is_empty() {
            return Err(vec![format!("{name} must not be empty")]);
        }
    }
    let mut exceptions = read_exceptions(&read(root, EXCEPTIONS)?)?;
    println!(
        "check-size: register entry added: {} up to {} lines ({}; approved: {}; {})",
        entry.item, entry.lines, entry.reason, entry.approved, entry.date
    );
    exceptions.push(entry);
    write(root, EXCEPTIONS, &write_exceptions(&exceptions))
}

/// Measures the tree under `root`. Only items over a limit are returned.
pub fn measure(root: &Path) -> Result<Sizes, Vec<String>> {
    let mut sizes = Sizes::default();
    for dir in ["crates", "xtask"] {
        walk(root, &root.join(dir), &mut sizes)?;
    }
    Ok(sizes)
}

fn walk(root: &Path, dir: &Path, sizes: &mut Sizes) -> Result<(), Vec<String>> {
    let entries = fs::read_dir(dir).map_err(|e| vec![format!("read {}: {e}", dir.display())])?;
    let mut paths: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if path.is_dir() {
            if !SKIPPED_DIRS.contains(&name) {
                walk(root, &path, sizes)?;
            }
        } else if name.ends_with(".rs") {
            let text = fs::read_to_string(&path)
                .map_err(|e| vec![format!("read {}: {e}", path.display())])?;
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            measure_file(&rel, &text, sizes);
        }
    }
    Ok(())
}

/// Adds the file and its functions to `sizes` when they are over the limits.
pub fn measure_file(rel: &str, text: &str, sizes: &mut Sizes) {
    let lines = u32::try_from(text.lines().count()).unwrap_or(u32::MAX);
    if lines > FILE_LIMIT {
        sizes.files.insert(rel.to_owned(), lines);
    }
    for (name, len) in functions(text) {
        if len > FN_LIMIT {
            let key = format!("{rel}::{name}");
            let entry = sizes.functions.entry(key).or_insert(0);
            *entry = (*entry).max(len);
        }
    }
}

/// Every function in `text` with its length in lines (the `fn` line to the closing
/// brace). Declarations without a body are left out.
pub fn functions(text: &str) -> Vec<(String, u32)> {
    let code = strip(text);
    let lines: Vec<&str> = code.lines().collect();
    let mut out = Vec::new();
    for (start, line) in lines.iter().enumerate() {
        let Some(name) = fn_name(line) else {
            continue;
        };
        let mut depth = 0i64;
        let mut opened = false;
        'scan: for (offset, body) in lines[start..].iter().enumerate() {
            for c in body.chars() {
                match c {
                    '{' => {
                        depth += 1;
                        opened = true;
                    }
                    '}' => depth -= 1,
                    ';' if !opened && depth == 0 => break 'scan,
                    _ => {}
                }
                if opened && depth == 0 {
                    out.push((name, u32::try_from(offset + 1).unwrap_or(u32::MAX)));
                    break 'scan;
                }
            }
        }
    }
    out
}

/// The function name when `line` starts a function item.
fn fn_name(line: &str) -> Option<String> {
    let mut rest = line.trim_start();
    loop {
        let before = rest;
        for prefix in [
            "pub(crate) ",
            "pub(super) ",
            "pub ",
            "const ",
            "async ",
            "unsafe ",
        ] {
            rest = rest.strip_prefix(prefix).unwrap_or(rest);
        }
        if let Some(after) = rest.strip_prefix("extern ") {
            rest = after.trim_start_matches(|c: char| c != ' ').trim_start();
        }
        if rest == before {
            break;
        }
    }
    let rest = rest.strip_prefix("fn ")?;
    let name: String = rest
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    (!name.is_empty()).then_some(name)
}

/// `text` with comments, string and character literals replaced by spaces; line breaks
/// are kept so line numbers stay.
fn strip(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let keep = |c: char, out: &mut String| out.push(if c == '\n' { '\n' } else { ' ' });
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        if c == '/' && next == Some('/') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && next == Some('*') {
            let mut depth = 0;
            while i < chars.len() {
                if chars[i] == '/' && chars.get(i + 1) == Some(&'*') {
                    depth += 1;
                    i += 2;
                } else if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    keep(chars[i], &mut out);
                    i += 1;
                }
            }
        } else if c == 'r' && (next == Some('"') || next == Some('#')) && raw_start(&chars, i) {
            // A raw string: r"..." or r#"..."#.
            let mut j = i + 1;
            let mut hashes = 0;
            while chars.get(j) == Some(&'#') {
                hashes += 1;
                j += 1;
            }
            i = j + 1;
            while i < chars.len() {
                if chars[i] == '"' && (1..=hashes).all(|k| chars.get(i + k) == Some(&'#')) {
                    i += 1 + hashes;
                    break;
                }
                keep(chars[i], &mut out);
                i += 1;
            }
        } else if c == '"' {
            i += 1;
            while i < chars.len() && chars[i] != '"' {
                if chars[i] == '\\' {
                    i += 1;
                }
                if let Some(&ch) = chars.get(i) {
                    keep(ch, &mut out);
                }
                i += 1;
            }
            i += 1;
        } else if c == '\'' {
            // A character literal ('{', '\'', '\u{7b}'); a lifetime has no closing quote
            // within a few characters.
            let close = (2..=10).find(|&k| {
                chars.get(i + k) == Some(&'\'') && (k == 2 || chars.get(i + 1) == Some(&'\\'))
            });
            match close {
                Some(k) => i += k + 1,
                None => {
                    out.push(c);
                    i += 1;
                }
            }
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

/// True when the `r` at `i` starts a raw string, not the end of an identifier.
fn raw_start(chars: &[char], i: usize) -> bool {
    let prev_ident = i > 0 && (chars[i - 1].is_alphanumeric() || chars[i - 1] == '_');
    if prev_ident {
        return false;
    }
    let mut j = i + 1;
    while chars.get(j) == Some(&'#') {
        j += 1;
    }
    chars.get(j) == Some(&'"')
}

fn read(root: &Path, rel: &str) -> Result<String, Vec<String>> {
    fs::read_to_string(root.join(rel)).map_err(|e| vec![format!("read {rel}: {e}")])
}

fn write(root: &Path, rel: &str, text: &str) -> Result<(), Vec<String>> {
    fs::write(root.join(rel), text).map_err(|e| vec![format!("write {rel}: {e}")])
}

/// Parses `size-baseline.toml`.
pub fn read_baseline(text: &str) -> Result<Sizes, Vec<String>> {
    let value: Value = text
        .parse()
        .map_err(|e| vec![format!("parse {BASELINE}: {e}")])?;
    let table = |key: &str| -> Result<BTreeMap<String, u32>, Vec<String>> {
        let mut out = BTreeMap::new();
        if let Some(t) = value.get(key).and_then(Value::as_table) {
            for (item, lines) in t {
                let lines = lines
                    .as_integer()
                    .and_then(|n| u32::try_from(n).ok())
                    .ok_or_else(|| vec![format!("{BASELINE}: {item} is not a line count")])?;
                out.insert(item.clone(), lines);
            }
        }
        Ok(out)
    };
    Ok(Sizes {
        files: table("files")?,
        functions: table("functions")?,
    })
}

/// Parses `size-exceptions.toml`.
pub fn read_exceptions(text: &str) -> Result<Vec<Exception>, Vec<String>> {
    let value: Value = text
        .parse()
        .map_err(|e| vec![format!("parse {EXCEPTIONS}: {e}")])?;
    let mut out = Vec::new();
    for entry in value
        .get("exception")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let field = |key: &str| {
            entry
                .get(key)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| vec![format!("{EXCEPTIONS}: an entry has no {key}")])
        };
        let lines = entry
            .get("lines")
            .and_then(Value::as_integer)
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| vec![format!("{EXCEPTIONS}: an entry has no lines")])?;
        out.push(Exception {
            item: field("item")?,
            lines,
            reason: field("reason")?,
            approved: field("approved")?,
            date: field("date")?,
        });
    }
    Ok(out)
}

fn quote(text: &str) -> String {
    Value::String(text.to_owned()).to_string()
}

/// The text of `size-baseline.toml`.
pub fn write_baseline(sizes: &Sizes) -> String {
    let mut out = format!(
        "# Line ceilings for `cargo xtask check-size` (quality plan WP-03). Do not edit by hand.\n\
         # A file over {FILE_LIMIT} lines or a function over {FN_LIMIT} lines is listed here and\n\
         # may not grow. A lower number is written with `cargo xtask check-size --lower`; an\n\
         # increase the owner approved goes to size-exceptions.toml with `--except`.\n\n[files]\n"
    );
    for (item, lines) in &sizes.files {
        out.push_str(&format!("{} = {lines}\n", quote(item)));
    }
    out.push_str("\n[functions]\n");
    for (item, lines) in &sizes.functions {
        out.push_str(&format!("{} = {lines}\n", quote(item)));
    }
    out
}

/// The text of `size-exceptions.toml`.
pub fn write_exceptions(exceptions: &[Exception]) -> String {
    let mut out = String::from(
        "# Approved size increases for `cargo xtask check-size`. Do not edit by hand.\n\
         # An entry is added with `cargo xtask check-size --except` after the owner approves\n\
         # in writing; `--lower` removes it once the item no longer needs it.\n",
    );
    for e in exceptions {
        out.push_str(&format!(
            "\n[[exception]]\nitem = {}\nlines = {}\nreason = {}\napproved = {}\ndate = {}\n",
            quote(&e.item),
            e.lines,
            quote(&e.reason),
            quote(&e.approved),
            quote(&e.date)
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn long_fn(name: &str, body: usize) -> String {
        let mut text = format!("pub fn {name}() {{\n");
        for _ in 0..body {
            text.push_str("    let x = \"}\"; // }\n");
        }
        text.push_str("}\n");
        text
    }

    #[test]
    fn a_function_is_measured_from_fn_to_its_closing_brace() {
        let text = format!("fn a() {{}}\n{}fn b();\n", long_fn("big", 3));
        let found = functions(&text);
        assert_eq!(found, vec![("a".to_owned(), 1), ("big".to_owned(), 5)]);
    }

    #[test]
    fn braces_in_strings_chars_and_comments_are_ignored() {
        let text =
            "fn a<'x>(s: &'x str) {\n    let c = '{';\n    let r = r#\"{\"#;\n    /* { */\n}\n";
        assert_eq!(functions(text), vec![("a".to_owned(), 5)]);
    }

    #[test]
    fn qualifiers_before_fn_are_read() {
        assert_eq!(
            fn_name("    pub(crate) async unsafe fn go()"),
            Some("go".into())
        );
        assert_eq!(fn_name("extern \"C\" fn cb()"), Some("cb".into()));
        assert_eq!(fn_name("let f = fn_ptr;"), None);
    }

    #[test]
    fn growth_past_a_ceiling_fails_and_room_below_it_must_be_written_down() {
        let mut found = Sizes::default();
        found.files.insert("a.rs".into(), 2100);
        let mut base = Sizes::default();
        base.files.insert("a.rs".into(), 2050);
        assert_eq!(compare(&found, &base, &[]).len(), 1);
        base.files.insert("a.rs".into(), 2200);
        assert!(compare(&found, &base, &[])[0].contains("--lower"));
        base.files.insert("a.rs".into(), 2100);
        assert!(compare(&found, &base, &[]).is_empty());
    }

    #[test]
    fn an_unlisted_item_over_the_limit_fails_unless_an_exception_allows_it() {
        let mut found = Sizes::default();
        found.functions.insert("a.rs::f".into(), 250);
        let base = Sizes::default();
        assert_eq!(compare(&found, &base, &[]).len(), 1);
        let ex = Exception {
            item: "a.rs::f".into(),
            lines: 250,
            reason: "r".into(),
            approved: "owner".into(),
            date: "2026-11-20".into(),
        };
        assert!(compare(&found, &base, &[ex]).is_empty());
    }

    #[test]
    fn the_files_round_trip() {
        let mut sizes = Sizes::default();
        sizes.files.insert("crates/a/src/lib.rs".into(), 2001);
        sizes
            .functions
            .insert("crates/a/src/lib.rs::go".into(), 201);
        assert_eq!(read_baseline(&write_baseline(&sizes)), Ok(sizes));
        let ex = vec![Exception {
            item: "x".into(),
            lines: 3,
            reason: "a \"quoted\" reason".into(),
            approved: "owner".into(),
            date: "2026-11-20".into(),
        }];
        assert_eq!(read_exceptions(&write_exceptions(&ex)), Ok(ex));
    }

    /// The tree stays within its ceilings (runs with `cargo test --workspace`).
    #[test]
    fn the_tree_is_within_its_ceilings() {
        let root = crate::workspace_root();
        if let Err(errors) = check(&root) {
            panic!("check-size:\n{}", errors.join("\n"));
        }
    }
}
