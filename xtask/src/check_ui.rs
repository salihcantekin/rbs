//! Counted UI checks of ADR-0115 (`cargo xtask check-ui`).
//!
//! The checks read the `.slint` files under `crates/rbs-app/ui` as text and count three
//! things over the whole tree. Each count must equal its number in `docs/ui/baseline.toml`
//! plus the approved entries in `docs/ui/exceptions.toml`. `--lower` writes lower counts
//! down, `--except` records an increase the owner approved, and `--against <commit>`
//! compares both files with an older commit before a merge.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use toml::Value;

/// The checks, in the order they are printed and stored.
pub const CHECKS: &[&str] = &["touch", "duplicate", "help"];

/// The rule id of a register entry that allows a new library file.
const LIBRARY_RULE: &str = "library";

/// A string literal longer than this in a `help:` statement belongs in an `InfoHint`.
const HELP_MAX_CHARS: usize = 90;

const UI_DIR: &str = "crates/rbs-app/ui";
const BASELINE: &str = "docs/ui/baseline.toml";
const EXCEPTIONS: &str = "docs/ui/exceptions.toml";

/// One approved increase or broken rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Exception {
    pub check: String,
    pub count: u32,
    pub file: String,
    pub reason: String,
    pub date: String,
    pub approved: String,
}

/// `docs/ui/baseline.toml`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Baseline {
    pub library: Vec<String>,
    pub counts: BTreeMap<String, u32>,
}

/// Runs `cargo xtask check-ui` with the arguments after the command name.
pub fn run(root: &Path, args: &[String]) -> Result<(), Vec<String>> {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        [] => check(root),
        ["--lower"] => lower(root),
        ["--against", base] => against(root, base),
        ["--hook", when] => hook(root, when),
        ["--except", check, count, file, reason, approved, date] => {
            let count = count
                .parse::<u32>()
                .map_err(|_| vec![format!("count must be a whole number, got {count}")])?;
            except(
                root,
                Exception {
                    check: (*check).to_owned(),
                    count,
                    file: (*file).to_owned(),
                    reason: (*reason).to_owned(),
                    date: (*date).to_owned(),
                    approved: (*approved).to_owned(),
                },
            )
        }
        _ => Err(vec![USAGE.to_owned()]),
    }
}

pub const USAGE: &str = "usage: cargo xtask check-ui
       cargo xtask check-ui --lower
       cargo xtask check-ui --against <commit>
       cargo xtask check-ui --except <check> <count> <file> <reason> <owner's approval> <date>
       cargo xtask check-ui --hook pre|post   (an agent's edit hook; reads the hook's JSON on stdin)";

/// An agent's edit hook (ADR-0115 decision 6). `pre` refuses a direct edit to the two
/// guarded files; `post` runs the checks after an edit to a `.slint` or token file.
/// Any other file passes without work.
fn hook(root: &Path, when: &str) -> Result<(), Vec<String>> {
    let mut input = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut input)
        .map_err(|e| vec![format!("read the hook input: {e}")])?;
    let path = edited_path(&input).unwrap_or_default().replace('\\', "/");
    match when {
        "pre" if path.ends_with(BASELINE) || path.ends_with(EXCEPTIONS) => Err(vec![format!(
            "{path} is not edited by hand (ADR-0115). A lower count: `cargo xtask check-ui \
             --lower`. A higher count: stop, ask the owner in writing, then `cargo xtask \
             check-ui --except`."
        )]),
        "post" if path.ends_with(".slint") || path.ends_with("crates/rbs-ui/tokens.toml") => {
            check(root)
        }
        "pre" | "post" => Ok(()),
        _ => Err(vec![USAGE.to_owned()]),
    }
}

/// The first `file_path` (or Cursor's `path`) string in an edit hook's JSON input.
pub fn edited_path(json: &str) -> Option<String> {
    let start = ["\"file_path\"", "\"path\""]
        .iter()
        .find_map(|key| json.find(key).map(|at| at + key.len()))?;
    let rest = json[start..].trim_start().strip_prefix(':')?.trim_start();
    let mut chars = rest.strip_prefix('"')?.chars();
    let mut out = String::new();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => out.push(chars.next()?),
            _ => out.push(c),
        }
    }
    None
}

/// Fails unless every count equals its baseline plus the register.
pub fn check(root: &Path) -> Result<(), Vec<String>> {
    let baseline = read_baseline(&read(root, BASELINE)?)?;
    let exceptions = read_exceptions(&read(root, EXCEPTIONS)?)?;
    let counts = count_tree(root, &baseline.library)?;
    let mut errors = Vec::new();
    for check in CHECKS {
        let found = counts.get(*check).copied().unwrap_or(0);
        let allowed = allowed(&baseline, &exceptions, check);
        if found > allowed {
            errors.push(format!(
                "{check}: {found} found, {allowed} allowed. {} Stop and ask the owner \
                 (ADR-0115); an approved increase is recorded with `cargo xtask check-ui --except`.",
                meaning(check)
            ));
        } else if found < allowed {
            errors.push(format!(
                "{check}: {found} found, {allowed} allowed. Write the lower number down with \
                 `cargo xtask check-ui --lower`."
            ));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn meaning(check: &str) -> &'static str {
    match check {
        "touch" => "A raw `TouchArea` outside the library is a home-made control.",
        "duplicate" => "A component name is declared in more than one file.",
        "help" => "A `help:` text over 90 characters belongs in an `InfoHint` (ADR-0094).",
        _ => "",
    }
}

fn allowed(baseline: &Baseline, exceptions: &[Exception], check: &str) -> u32 {
    let base = baseline.counts.get(check).copied().unwrap_or(0);
    let extra: u32 = exceptions
        .iter()
        .filter(|e| e.check == check)
        .map(|e| e.count)
        .sum();
    base.saturating_add(extra)
}

fn lower(root: &Path) -> Result<(), Vec<String>> {
    let mut baseline = read_baseline(&read(root, BASELINE)?)?;
    let mut exceptions = read_exceptions(&read(root, EXCEPTIONS)?)?;
    let counts = count_tree(root, &baseline.library)?;
    let mut errors = Vec::new();
    for check in CHECKS {
        let found = counts.get(*check).copied().unwrap_or(0);
        let allowed_now = allowed(&baseline, &exceptions, check);
        if found > allowed_now {
            errors.push(format!(
                "{check}: {found} found, {allowed_now} allowed. --lower never raises a number."
            ));
            continue;
        }
        let mut surplus = allowed_now - found;
        // The register first, oldest entry first, then the baseline.
        for entry in exceptions.iter_mut().filter(|e| e.check == *check) {
            let taken = surplus.min(entry.count);
            if taken > 0 {
                entry.count -= taken;
                surplus -= taken;
                println!(
                    "check-ui: register entry for {check} in {} reduced by {taken}",
                    entry.file
                );
            }
        }
        if surplus > 0 {
            let base = baseline.counts.entry((*check).to_owned()).or_insert(0);
            *base = base.saturating_sub(surplus);
            println!("check-ui: baseline {check} lowered to {base}");
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    exceptions.retain(|e| e.count > 0 || !CHECKS.contains(&e.check.as_str()));
    write(root, BASELINE, &write_baseline(&baseline))?;
    write(root, EXCEPTIONS, &write_exceptions(&exceptions))
}

fn except(root: &Path, entry: Exception) -> Result<(), Vec<String>> {
    if !CHECKS.contains(&entry.check.as_str()) && entry.check != LIBRARY_RULE {
        return Err(vec![format!(
            "unknown check {}; use touch, duplicate, help or library",
            entry.check
        )]);
    }
    for (name, value) in [
        ("file", &entry.file),
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
        "check-ui: register entry added: {} +{} in {} ({}; approved: {}; {})",
        entry.check, entry.count, entry.file, entry.reason, entry.approved, entry.date
    );
    exceptions.push(entry);
    write(root, EXCEPTIONS, &write_exceptions(&exceptions))
}

fn against(root: &Path, base: &str) -> Result<(), Vec<String>> {
    check(root)?;
    let baseline = read_baseline(&read(root, BASELINE)?)?;
    let exceptions = read_exceptions(&read(root, EXCEPTIONS)?)?;
    let (Some(old_baseline), Some(old_exceptions)) = (
        git_show(root, base, BASELINE)?,
        git_show(root, base, EXCEPTIONS)?,
    ) else {
        println!("check-ui: {base} has no baseline or register yet; nothing to compare.");
        return Ok(());
    };
    let old_baseline = read_baseline(&old_baseline)?;
    let old_exceptions = read_exceptions(&old_exceptions)?;
    let errors = compare(&old_baseline, &old_exceptions, &baseline, &exceptions);
    let added: Vec<&Exception> = exceptions
        .iter()
        .filter(|e| !old_exceptions.contains(e))
        .collect();
    if added.is_empty() {
        println!("check-ui: UI exceptions added since {base}: none");
    } else {
        println!("check-ui: UI exceptions added since {base}:");
        for e in added {
            println!(
                "  {} +{} in {}: {} (approved: {}; {})",
                e.check, e.count, e.file, e.reason, e.approved, e.date
            );
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// What `--against` refuses: a baseline number above the older one, and a library file
/// that the older list does not have and no register entry allows.
pub fn compare(
    old_baseline: &Baseline,
    old_exceptions: &[Exception],
    baseline: &Baseline,
    exceptions: &[Exception],
) -> Vec<String> {
    let mut errors = Vec::new();
    for check in CHECKS {
        let old = old_baseline.counts.get(*check).copied().unwrap_or(0);
        let new = baseline.counts.get(*check).copied().unwrap_or(0);
        if new > old {
            errors.push(format!(
                "baseline {check} rose from {old} to {new}; a count rises only through the register"
            ));
        }
    }
    for file in &baseline.library {
        if old_baseline.library.contains(file) {
            continue;
        }
        let approved = exceptions
            .iter()
            .any(|e| e.check == LIBRARY_RULE && &e.file == file && !old_exceptions.contains(e));
        if !approved {
            errors.push(format!(
                "library file {file} was added without a register entry of rule `library`"
            ));
        }
    }
    errors
}

fn git_show(root: &Path, base: &str, path: &str) -> Result<Option<String>, Vec<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["show", &format!("{base}:{path}")])
        .output()
        .map_err(|e| vec![format!("run git: {e}")])?;
    if !output.status.success() {
        return Ok(None);
    }
    String::from_utf8(output.stdout)
        .map(Some)
        .map_err(|e| vec![format!("{path} at {base}: {e}")])
}

fn read(root: &Path, path: &str) -> Result<String, Vec<String>> {
    let full = root.join(path);
    fs::read_to_string(&full).map_err(|e| vec![format!("read {}: {e}", full.display())])
}

fn write(root: &Path, path: &str, text: &str) -> Result<(), Vec<String>> {
    let full = root.join(path);
    fs::write(&full, text).map_err(|e| vec![format!("write {}: {e}", full.display())])
}

pub fn read_baseline(text: &str) -> Result<Baseline, Vec<String>> {
    let value: Value = text.parse().map_err(|e| vec![format!("{BASELINE}: {e}")])?;
    let library = value
        .get("library")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            vec![format!(
                "{BASELINE}: `library` must be a list of file names"
            )]
        })?
        .iter()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect();
    let table = value
        .get("baseline")
        .and_then(Value::as_table)
        .ok_or_else(|| vec![format!("{BASELINE}: missing [baseline]")])?;
    let mut counts = BTreeMap::new();
    for check in CHECKS {
        let number = table
            .get(*check)
            .and_then(Value::as_integer)
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| vec![format!("{BASELINE}: `{check}` must be a number from 0 up")])?;
        counts.insert((*check).to_owned(), number);
    }
    Ok(Baseline { library, counts })
}

pub fn read_exceptions(text: &str) -> Result<Vec<Exception>, Vec<String>> {
    let value: Value = text
        .parse()
        .map_err(|e| vec![format!("{EXCEPTIONS}: {e}")])?;
    let Some(list) = value.get("exception") else {
        return Ok(Vec::new());
    };
    let list = list.as_array().ok_or_else(|| {
        vec![format!(
            "{EXCEPTIONS}: `exception` must be a list of tables"
        )]
    })?;
    let mut out = Vec::new();
    for (index, item) in list.iter().enumerate() {
        let field = |name: &str| -> Result<String, Vec<String>> {
            item.get(name)
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .map(str::to_owned)
                .ok_or_else(|| {
                    vec![format!(
                        "{EXCEPTIONS}: entry {}: `{name}` is missing",
                        index + 1
                    )]
                })
        };
        let count = item
            .get("count")
            .and_then(Value::as_integer)
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| {
                vec![format!(
                    "{EXCEPTIONS}: entry {}: `count` must be a number from 0 up",
                    index + 1
                )]
            })?;
        out.push(Exception {
            check: field("check")?,
            count,
            file: field("file")?,
            reason: field("reason")?,
            date: field("date")?,
            approved: field("approved")?,
        });
    }
    Ok(out)
}

const BASELINE_HEAD: &str = "\
# Allowed counts for `cargo xtask check-ui` (ADR-0115). Do not edit by hand.
# A lower count is written down with `cargo xtask check-ui --lower`.
# A count rises only through docs/ui/exceptions.toml, after the owner approves.
";

const EXCEPTIONS_HEAD: &str = "\
# Approved UI exceptions (ADR-0115). Do not edit by hand.
# An entry is added with `cargo xtask check-ui --except` after the owner approves in writing.
";

pub fn write_baseline(baseline: &Baseline) -> String {
    let library: Vec<String> = baseline.library.iter().map(|f| quote(f)).collect();
    let mut out = format!(
        "{BASELINE_HEAD}\n# Files that hold library components. They are not counted by `touch` and `help`.\n\
         library = [{}]\n\n[baseline]\n",
        library.join(", ")
    );
    for check in CHECKS {
        let number = baseline.counts.get(*check).copied().unwrap_or(0);
        out.push_str(&format!("{check} = {number}\n"));
    }
    out
}

pub fn write_exceptions(exceptions: &[Exception]) -> String {
    let mut out = EXCEPTIONS_HEAD.to_owned();
    for e in exceptions {
        out.push_str(&format!(
            "\n[[exception]]\ncheck = {}\ncount = {}\nfile = {}\nreason = {}\ndate = {}\napproved = {}\n",
            quote(&e.check),
            e.count,
            quote(&e.file),
            quote(&e.reason),
            quote(&e.date),
            quote(&e.approved)
        ));
    }
    out
}

fn quote(text: &str) -> String {
    Value::String(text.to_owned()).to_string()
}

/// Counts every check over the `.slint` files under the UI folder.
pub fn count_tree(root: &Path, library: &[String]) -> Result<BTreeMap<String, u32>, Vec<String>> {
    let dir = root.join(UI_DIR);
    let mut paths: Vec<PathBuf> = Vec::new();
    collect(&dir, &mut paths)?;
    paths.sort();
    let mut files = Vec::new();
    for path in paths {
        let name = path
            .strip_prefix(&dir)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let text =
            fs::read_to_string(&path).map_err(|e| vec![format!("read {}: {e}", path.display())])?;
        files.push((name, text));
    }
    Ok(count_files(&files, library))
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), Vec<String>> {
    let entries = fs::read_dir(dir).map_err(|e| vec![format!("read {}: {e}", dir.display())])?;
    for entry in entries {
        let path = entry.map_err(|e| vec![e.to_string()])?.path();
        if path.is_dir() {
            collect(&path, out)?;
        } else if path.extension().is_some_and(|e| e == "slint") {
            out.push(path);
        }
    }
    Ok(())
}

/// Counts every check over `(file name, text)` pairs.
pub fn count_files(files: &[(String, String)], library: &[String]) -> BTreeMap<String, u32> {
    let mut touch = 0;
    let mut help = 0;
    let mut declared: BTreeMap<String, BTreeSet<&str>> = BTreeMap::new();
    let mut declarations: Vec<String> = Vec::new();
    for (name, text) in files {
        let code = Code::new(text);
        for component in code.components() {
            declared.entry(component.clone()).or_default().insert(name);
            declarations.push(component);
        }
        if library.contains(name) {
            continue;
        }
        touch += code.touch_areas();
        help += code.long_help();
    }
    let duplicate = declarations
        .iter()
        .filter(|name| declared.get(*name).is_some_and(|files| files.len() > 1))
        .count();
    let mut counts = BTreeMap::new();
    counts.insert("touch".to_owned(), touch);
    counts.insert(
        "duplicate".to_owned(),
        u32::try_from(duplicate).unwrap_or(u32::MAX),
    );
    counts.insert("help".to_owned(), help);
    counts
}

/// One piece of a `.slint` file with comments removed.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Token {
    /// An identifier or keyword, with its dashes.
    Word(String),
    /// A string literal's length in characters.
    Text(usize),
    /// Any other single character that is not white space.
    Mark(char),
}

struct Code {
    tokens: Vec<Token>,
}

impl Code {
    fn new(text: &str) -> Self {
        let chars: Vec<char> = text.chars().collect();
        let mut tokens = Vec::new();
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            let next = chars.get(i + 1).copied();
            if c == '/' && next == Some('/') {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            } else if c == '/' && next == Some('*') {
                i += 2;
                while i < chars.len() && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                    i += 1;
                }
                i += 2;
            } else if c == '"' {
                i += 1;
                let mut len = 0;
                while i < chars.len() && chars[i] != '"' {
                    if chars[i] == '\\' {
                        i += 1;
                    }
                    len += 1;
                    i += 1;
                }
                i += 1;
                tokens.push(Token::Text(len));
            } else if c.is_alphabetic() || c == '_' {
                let start = i;
                while i < chars.len()
                    && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '-')
                {
                    i += 1;
                }
                tokens.push(Token::Word(chars[start..i].iter().collect()));
            } else {
                if !c.is_whitespace() {
                    tokens.push(Token::Mark(c));
                }
                i += 1;
            }
        }
        Self { tokens }
    }

    fn word(&self, index: usize) -> Option<&str> {
        match self.tokens.get(index) {
            Some(Token::Word(word)) => Some(word),
            _ => None,
        }
    }

    fn mark(&self, index: usize) -> Option<char> {
        match self.tokens.get(index) {
            Some(Token::Mark(mark)) => Some(*mark),
            _ => None,
        }
    }

    /// Names declared with `component Name`.
    fn components(&self) -> Vec<String> {
        (0..self.tokens.len())
            .filter(|i| self.word(*i) == Some("component"))
            .filter_map(|i| self.word(i + 1).map(str::to_owned))
            .collect()
    }

    /// `TouchArea {` elements and `inherits TouchArea`.
    fn touch_areas(&self) -> u32 {
        let found = (0..self.tokens.len())
            .filter(|i| self.word(*i) == Some("TouchArea"))
            .filter(|i| {
                self.mark(i + 1) == Some('{') || (*i > 0 && self.word(i - 1) == Some("inherits"))
            })
            .count();
        u32::try_from(found).unwrap_or(u32::MAX)
    }

    /// `help:` statements, up to their `;`, that hold a string literal over the limit.
    fn long_help(&self) -> u32 {
        let mut found = 0;
        let mut i = 0;
        while i < self.tokens.len() {
            let starts = self.word(i) == Some("help")
                && self.mark(i + 1) == Some(':')
                && !(i > 0 && self.mark(i - 1) == Some('.'));
            if !starts {
                i += 1;
                continue;
            }
            let mut depth = 0_i32;
            let mut long = false;
            i += 2;
            while i < self.tokens.len() {
                match &self.tokens[i] {
                    Token::Mark('(' | '[' | '{') => depth += 1,
                    Token::Mark(')' | ']') => depth -= 1,
                    Token::Mark('}') => {
                        depth -= 1;
                        if depth < 0 {
                            break;
                        }
                    }
                    Token::Mark(';') if depth <= 0 => break,
                    Token::Text(len) if *len > HELP_MAX_CHARS => long = true,
                    _ => {}
                }
                i += 1;
            }
            if long {
                found += 1;
            }
        }
        found
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    fn counts(files: &[(&str, &str)]) -> BTreeMap<String, u32> {
        let files: Vec<(String, String)> = files
            .iter()
            .map(|(n, t)| ((*n).to_owned(), (*t).to_owned()))
            .collect();
        count_files(&files, &["components.slint".to_owned()])
    }

    #[test]
    fn the_tree_matches_the_baseline_and_the_register() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        if let Err(errors) = check(&root) {
            panic!("cargo xtask check-ui:\n{}", errors.join("\n"));
        }
    }

    #[test]
    fn touch_counts_elements_and_inheritance_outside_the_library() {
        let c = counts(&[
            ("components.slint", "component A { TouchArea { } }"),
            (
                "panel.slint",
                "// TouchArea { in a comment }\n\
                 component B inherits TouchArea { }\n\
                 component C { ta := TouchArea { } Text { text: \"TouchArea { \"; } }",
            ),
        ]);
        assert_eq!(c["touch"], 2);
    }

    #[test]
    fn duplicate_counts_every_declaration_of_a_name_in_two_files() {
        let c = counts(&[
            ("components.slint", "export component Hint { }"),
            ("a.slint", "component Hint { } component Only { }"),
            ("b.slint", "component Hint { } component Other { }"),
        ]);
        assert_eq!(c["duplicate"], 3);
    }

    #[test]
    fn help_counts_one_per_statement_with_a_long_literal() {
        let long = "x".repeat(HELP_MAX_CHARS + 1);
        let short = "x".repeat(HELP_MAX_CHARS);
        let text = format!(
            "component P {{\n\
               SettingRow {{ help: @tr(\"{short}\"); }}\n\
               SettingRow {{ help: root.live\n ? @tr(\"{long}\")\n : @tr(\"{long}\"); }}\n\
               SettingRow {{ label: @tr(\"{long}\"); }}\n\
               SettingRow {{ help: @tr(\"{long}\") }}\n\
               Text {{ text: row.help; }}\n\
             }}"
        );
        let c = counts(&[("p.slint", &text)]);
        assert_eq!(c["help"], 2);
    }

    #[test]
    fn the_hook_finds_the_edited_file() {
        let json = r#"{"tool_name":"Edit","tool_input":{"file_path":"C:\\rbs\\docs\\ui\\baseline.toml","old_string":"a"}}"#;
        assert_eq!(
            edited_path(json).unwrap().replace('\\', "/"),
            "C:/rbs/docs/ui/baseline.toml"
        );
        assert_eq!(edited_path(r#"{"tool_input":{}}"#), None);
    }

    #[test]
    fn files_round_trip() {
        let baseline = Baseline {
            library: vec!["components.slint".to_owned()],
            counts: CHECKS.iter().map(|c| ((*c).to_owned(), 3)).collect(),
        };
        assert_eq!(read_baseline(&write_baseline(&baseline)).unwrap(), baseline);
        let exceptions = vec![Exception {
            check: "touch".to_owned(),
            count: 1,
            file: "studio.slint".to_owned(),
            reason: "a \"quoted\" reason".to_owned(),
            date: "2026-11-13".to_owned(),
            approved: "owner, in the thread".to_owned(),
        }];
        assert_eq!(
            read_exceptions(&write_exceptions(&exceptions)).unwrap(),
            exceptions
        );
        assert!(read_exceptions(&write_exceptions(&[])).unwrap().is_empty());
    }

    #[test]
    fn against_refuses_a_raised_baseline_and_an_unapproved_library_file() {
        let old = Baseline {
            library: vec!["components.slint".to_owned()],
            counts: CHECKS.iter().map(|c| ((*c).to_owned(), 3)).collect(),
        };
        let mut new = old.clone();
        assert!(compare(&old, &[], &new, &[]).is_empty());
        new.counts.insert("touch".to_owned(), 4);
        new.library.push("rows.slint".to_owned());
        assert_eq!(compare(&old, &[], &new, &[]).len(), 2);
        let entry = Exception {
            check: LIBRARY_RULE.to_owned(),
            count: 0,
            file: "rows.slint".to_owned(),
            reason: "library split".to_owned(),
            date: "2026-11-13".to_owned(),
            approved: "owner".to_owned(),
        };
        assert_eq!(compare(&old, &[], &new, &[entry]).len(), 1);
    }
}
