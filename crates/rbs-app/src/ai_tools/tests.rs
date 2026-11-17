use super::*;

use rbs_api::ConnectionId;
use rbs_api::ScopeSet;

/// Paths with spaces, quotes, backslashes, `$`, `%` and non-ASCII characters.
const AWKWARD: [&str; 6] = [
    "/Applications/RBS.app/Contents/MacOS/rbs-mcp",
    "/Users/Zoë Çelik/Library/Application Support/RBS/agent-token",
    r"C:\Program Files\RBS\rbs-mcp.exe",
    "/home/o'brien/\"quoted\"/$HOME/%APPDATA%/rbs-mcp",
    r"C:\Users\Şükrü\AppData\Roaming\RBS\agent-token",
    "/tmp/a`b`/c;d|e&f/rbs-mcp",
];

/// Splits a POSIX `sh` command line into words: unquoted words, single quotes and
/// the `'\''` idiom. Panics on anything else (the generator must not produce it).
fn sh_words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            ' ' => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            '\'' => {
                in_word = true;
                for c in chars.by_ref() {
                    if c == '\'' {
                        break;
                    }
                    word.push(c);
                }
            }
            '\\' => {
                in_word = true;
                word.push(chars.next().expect("an escaped character"));
            }
            c => {
                assert!(
                    c.is_ascii_alphanumeric() || "-_".contains(c),
                    "unquoted {c:?} in {line}"
                );
                in_word = true;
                word.push(c);
            }
        }
    }
    if in_word {
        words.push(word);
    }
    words
}

/// Splits a PowerShell command line: unquoted words and single-quoted strings, where
/// `''` is one quote. Inside single quotes PowerShell expands nothing.
fn ps_words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut chars = line.chars().peekable();
    while let Some(&c) = chars.peek() {
        match c {
            ' ' => {
                chars.next();
            }
            '\'' => {
                chars.next();
                let mut word = String::new();
                loop {
                    match chars.next().expect("a closing quote") {
                        '\'' if chars.peek() == Some(&'\'') => {
                            chars.next();
                            word.push('\'');
                        }
                        '\'' => break,
                        c => word.push(c),
                    }
                }
                words.push(word);
            }
            _ => {
                let mut word = String::new();
                while let Some(&c) = chars.peek() {
                    if c == ' ' {
                        break;
                    }
                    assert!(
                        c.is_ascii_alphanumeric() || "-_".contains(c),
                        "unquoted {c:?} in {line}"
                    );
                    word.push(c);
                    chars.next();
                }
                words.push(word);
            }
        }
    }
    words
}

#[test]
fn json_lines_parse_and_name_both_paths() {
    for mcp in AWKWARD {
        for token in AWKWARD {
            for tool in [Tool::ClaudeDesktop, Tool::Cursor, Tool::VsCode] {
                let l = line(tool, Path::new(mcp), Path::new(token), Shell::native());
                let v: serde_json::Value = serde_json::from_str(&l.text).expect("JSON");
                let servers = if tool == Tool::VsCode {
                    "servers"
                } else {
                    "mcpServers"
                };
                let rbs = &v[servers]["rbs"];
                assert_eq!(rbs["command"], mcp, "{}", l.text);
                assert_eq!(rbs["args"][0], "--token-file");
                assert_eq!(rbs["args"][1], token);
                assert_eq!(rbs["type"] == "stdio", tool == Tool::VsCode);
                assert_eq!(v.as_object().map(|o| o.len()), Some(1));
                assert!(l.help.contains(servers), "{}", l.help);
            }
        }
    }
}

#[test]
fn command_lines_parse_by_their_shell() {
    for mcp in AWKWARD {
        for token in AWKWARD {
            let l = line(Tool::Other, Path::new(mcp), Path::new(token), Shell::Sh);
            assert_eq!(sh_words(&l.text), [mcp, "--token-file", token]);
            let l = line(
                Tool::ClaudeCode,
                Path::new(mcp),
                Path::new(token),
                Shell::Sh,
            );
            assert_eq!(
                sh_words(&l.text),
                [
                    "claude",
                    "mcp",
                    "add",
                    "rbs",
                    "--",
                    mcp,
                    "--token-file",
                    token
                ]
            );
            let l = line(
                Tool::Other,
                Path::new(mcp),
                Path::new(token),
                Shell::PowerShell,
            );
            assert_eq!(ps_words(&l.text), [mcp, "--token-file", token]);
            let l = line(
                Tool::ClaudeCode,
                Path::new(mcp),
                Path::new(token),
                Shell::PowerShell,
            );
            assert_eq!(
                ps_words(&l.text),
                [
                    "claude",
                    "mcp",
                    "add",
                    "rbs",
                    "--",
                    mcp,
                    "--token-file",
                    token
                ]
            );
        }
    }
}

#[test]
fn no_line_contains_a_token() {
    let token = rbs_api::Token::from_bytes(&[3; 32]);
    for tool in TOOLS {
        for shell in [Shell::Sh, Shell::PowerShell] {
            let l = line(tool, Path::new(AWKWARD[0]), Path::new(AWKWARD[1]), shell);
            assert!(!l.text.contains("rbs1_"));
            assert!(!l.text.contains(token.expose()));
        }
    }
}

#[test]
fn a_sandboxed_or_missing_rbs_mcp_gives_a_note_and_no_line() {
    let env = |name: &'static str| move |v: &str| (v == name).then(|| "x".to_owned());
    assert_eq!(sandbox_from_env(env("APPIMAGE")), Some("an AppImage"));
    assert_eq!(sandbox_from_env(env("FLATPAK_ID")), Some("a Flatpak"));
    assert_eq!(sandbox_from_env(env("SNAP")), Some("a Snap"));
    assert_eq!(sandbox_from_env(|_| None), None);
    let why = locate_mcp(Path::new("/x/rbs"), Some("a Flatpak")).unwrap_err();
    assert!(why.contains("Flatpak"));
    let dir = std::env::temp_dir().join(format!("rbs-app-mcp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let exe = dir.join(if cfg!(windows) { "rbs.exe" } else { "rbs" });
    assert!(
        locate_mcp(&exe, None)
            .unwrap_err()
            .contains("not next to RBS")
    );
    let mcp = exe.with_file_name(if cfg!(windows) {
        "rbs-mcp.exe"
    } else {
        "rbs-mcp"
    });
    std::fs::write(&mcp, b"").unwrap();
    assert_eq!(locate_mcp(&exe, None).unwrap(), mcp);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_cargo_build_without_rbs_mcp_says_how_to_build_it() {
    let dir = std::env::temp_dir().join(format!("rbs-app-mcp-cargo-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let exe_name = if cfg!(windows) { "rbs.exe" } else { "rbs" };
    // `<target>/<profile>/rbs` and `<target>/<triple>/<profile>/rbs`.
    for profile in [
        dir.join("debug"),
        dir.join("aarch64-apple-darwin").join("release"),
    ] {
        std::fs::create_dir_all(&profile).unwrap();
        let exe = profile.join(exe_name);
        assert!(
            locate_mcp(&exe, None)
                .unwrap_err()
                .contains("not next to RBS"),
            "no CACHEDIR.TAG yet"
        );
        std::fs::write(dir.join("CACHEDIR.TAG"), b"").unwrap();
        let why = locate_mcp(&exe, None).unwrap_err();
        assert!(why.contains("cargo build -p rbs-mcp"), "{why}");
        std::fs::remove_file(dir.join("CACHEDIR.TAG")).unwrap();
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn config_files_are_where_each_tool_reads_them() {
    let env = |pairs: &'static [(&'static str, &'static str)]| {
        move |v: &str| {
            pairs
                .iter()
                .find(|(k, _)| *k == v)
                .map(|(_, x)| (*x).to_owned())
        }
    };
    let mac = env(&[("HOME", "/Users/z")]);
    assert_eq!(
        config_file(Tool::ClaudeDesktop, Os::Mac, mac),
        Some(PathBuf::from(
            "/Users/z/Library/Application Support/Claude/claude_desktop_config.json"
        ))
    );
    assert_eq!(
        config_file(Tool::VsCode, Os::Mac, mac),
        Some(PathBuf::from(
            "/Users/z/Library/Application Support/Code/User/mcp.json"
        ))
    );
    assert_eq!(
        config_file(Tool::Cursor, Os::Mac, mac),
        Some(PathBuf::from("/Users/z/.cursor/mcp.json"))
    );
    let win = env(&[
        ("USERPROFILE", "C:/Users/z"),
        ("APPDATA", "C:/Users/z/AppData/Roaming"),
    ]);
    assert_eq!(
        config_file(Tool::ClaudeDesktop, Os::Windows, win),
        Some(
            Path::new("C:/Users/z/AppData/Roaming")
                .join("Claude")
                .join("claude_desktop_config.json")
        )
    );
    assert_eq!(
        config_file(Tool::Cursor, Os::Windows, win),
        Some(Path::new("C:/Users/z").join(".cursor").join("mcp.json"))
    );
    let linux = env(&[("HOME", "/home/z")]);
    assert_eq!(
        config_file(Tool::ClaudeDesktop, Os::Linux, linux),
        Some(PathBuf::from(
            "/home/z/.config/Claude/claude_desktop_config.json"
        ))
    );
    let xdg = env(&[("HOME", "/home/z"), ("XDG_CONFIG_HOME", "/cfg")]);
    assert_eq!(
        config_file(Tool::VsCode, Os::Linux, xdg),
        Some(PathBuf::from("/cfg/Code/User/mcp.json"))
    );
    for os in [Os::Mac, Os::Windows, Os::Linux] {
        assert_eq!(config_file(Tool::ClaudeCode, os, mac), None);
        assert_eq!(config_file(Tool::Other, os, mac), None);
        assert_eq!(config_file(Tool::ClaudeDesktop, os, |_| None), None);
        assert_eq!(config_file(Tool::Cursor, os, env(&[("HOME", "")])), None);
    }
}

#[test]
fn client_names_are_cleaned() {
    assert_eq!(clean_client_name("Claude Code"), "Claude Code");
    assert_eq!(
        clean_client_name("evil\u{202E}txt.exe\nline\t2\u{2028}"),
        "eviltxt.exeline2"
    );
    assert_eq!(
        clean_client_name(&"x".repeat(200)).chars().count(),
        MAX_CLIENT_NAME
    );
    assert_eq!(clean_client_name("\u{2066}  \u{2069}"), "");
}

fn conn(n: u64) -> ConnectionId {
    ConnectionId::from_raw(n)
}

#[test]
fn the_ring_keeps_the_last_20_ai_tools_actions_and_the_connected_names() {
    let f = Forward::new();
    let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    {
        let hits = hits.clone();
        f.notify_with(move || {
            hits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        });
    }
    let irreversible = |kind| AuditEvent::Irreversible {
        conn: conn(1),
        token: Some("x".into()),
        kind,
        method: "record.start",
    };
    // No ring before the first Start: nothing is kept.
    f.record(&irreversible(Some(TokenKind::AiTools)), 1, true);
    assert!(f.read().0.is_empty());
    f.start();
    f.record(
        &AuditEvent::Authenticated {
            conn: conn(1),
            token: Some("AI tools".into()),
            kind: Some(TokenKind::AiTools),
            client: Some("Claude\u{202E} Code".into()),
            scopes: ScopeSet::PANEL,
        },
        1,
        true,
    );
    assert_eq!(f.read().1, ["Claude Code"]);
    // Another kind's calls are not listed.
    f.record(&irreversible(Some(TokenKind::User)), 2, true);
    f.record(&irreversible(None), 2, true);
    assert!(f.read().0.is_empty());
    for i in 0..25 {
        f.record(&irreversible(Some(TokenKind::AiTools)), 10 + i, true);
    }
    f.record(
        &AuditEvent::NotPermitted {
            conn: conn(1),
            token: None,
            kind: Some(TokenKind::AiTools),
            method: "scene.switch",
            reason: "on_air",
        },
        99,
        true,
    );
    let (actions, _) = f.read();
    assert_eq!(actions.len(), RING);
    assert_eq!(actions[0].method, "scene.switch");
    assert_eq!(actions[0].refused, Some("on_air"));
    assert_eq!(actions[RING - 1].at, 16, "the oldest kept");
    // Stopped: the ring is kept, no new records, nobody connected.
    f.stop();
    f.record(&irreversible(Some(TokenKind::AiTools)), 200, false);
    let (actions, clients) = f.read();
    assert_eq!(actions[0].at, 99);
    assert!(clients.is_empty());
    assert!(hits.load(std::sync::atomic::Ordering::Relaxed) >= 27);
    f.clear();
    assert!(f.read().0.is_empty());
}

#[test]
fn a_closed_connection_leaves_the_list() {
    let f = Forward::new();
    f.start();
    for n in [1, 2] {
        f.record(
            &AuditEvent::Authenticated {
                conn: conn(n),
                token: Some("AI tools".into()),
                kind: Some(TokenKind::AiTools),
                client: Some(format!("tool {n}")),
                scopes: ScopeSet::PANEL,
            },
            1,
            true,
        );
    }
    f.record(&AuditEvent::Disconnected { conn: conn(1) }, 2, true);
    assert_eq!(f.read().1, ["tool 2"]);
}
