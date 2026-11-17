//! AI tools (Settings, AI tools; ADR-0134): the connection line for each AI tool
//! (decision 5), the names connected AI tools report, cleaned (decision 7), and the
//! ring of recent actions fed from the control API's audit (decision 7). Toolkit-free
//! and tested without a window; the page itself is wired in `studio_ui`.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use rbs_api::{AuditEvent, TokenKind};

/// The AI tools the page makes a line for, in its drop-down's order.
pub const TOOLS: [Tool; 5] = [
    Tool::ClaudeDesktop,
    Tool::ClaudeCode,
    Tool::Cursor,
    Tool::VsCode,
    Tool::Other,
];

/// An AI tool the connection line is made for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    /// `claude_desktop_config.json`, under `mcpServers`.
    ClaudeDesktop,
    /// A `claude mcp add` command.
    ClaudeCode,
    /// `mcp.json`, under `mcpServers`.
    Cursor,
    /// `.vscode/mcp.json`, under `servers`.
    VsCode,
    /// The command itself.
    Other,
}

impl Tool {
    /// The name in the drop-down.
    pub fn label(self) -> &'static str {
        match self {
            Tool::ClaudeDesktop => "Claude Desktop",
            Tool::ClaudeCode => "Claude Code",
            Tool::Cursor => "Cursor",
            Tool::VsCode => "VS Code",
            Tool::Other => "Other",
        }
    }

    /// The tool at `index` of [`TOOLS`]; Claude Desktop when out of range.
    pub fn at(index: i32) -> Tool {
        usize::try_from(index)
            .ok()
            .and_then(|i| TOOLS.get(i).copied())
            .unwrap_or(Tool::ClaudeDesktop)
    }
}

/// The shell a command line is pasted into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    /// POSIX `sh` (macOS, Linux).
    Sh,
    /// PowerShell (Windows).
    PowerShell,
}

impl Shell {
    /// This computer's shell.
    pub fn native() -> Shell {
        if cfg!(windows) {
            Shell::PowerShell
        } else {
            Shell::Sh
        }
    }

    /// `text` as one word of this shell.
    pub fn quote(self, text: &str) -> String {
        match self {
            Shell::Sh => format!("'{}'", text.replace('\'', r"'\''")),
            Shell::PowerShell => format!("'{}'", text.replace('\'', "''")),
        }
    }
}

/// Where the line goes and the line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    /// Where to paste it ("Add to claude_desktop_config.json, under mcpServers").
    pub place: String,
    /// The text to copy.
    pub text: String,
}

/// The line for `tool`, naming `mcp` and `token_file` by their absolute paths. Paths
/// are inserted by code: JSON-escaped, or quoted for `shell`. The token never appears.
pub fn line(tool: Tool, mcp: &Path, token_file: &Path, shell: Shell) -> Line {
    let mcp_text = mcp.to_string_lossy();
    let token_text = token_file.to_string_lossy();
    let json = |s: &str| serde_json::Value::String(s.to_owned()).to_string();
    let entry = |vs_code: bool| {
        format!(
            r#""rbs": {{{}"command": {}, "args": ["--token-file", {}]}}"#,
            if vs_code { r#""type": "stdio", "# } else { "" },
            json(&mcp_text),
            json(&token_text)
        )
    };
    let command = format!(
        "{} --token-file {}",
        shell.quote(&mcp_text),
        shell.quote(&token_text)
    );
    match tool {
        Tool::ClaudeDesktop => Line {
            place: "Add to claude_desktop_config.json, under mcpServers".into(),
            text: entry(false),
        },
        Tool::Cursor => Line {
            place: "Add to mcp.json (in .cursor), under mcpServers".into(),
            text: entry(false),
        },
        Tool::VsCode => Line {
            place: "Add to .vscode/mcp.json, under servers".into(),
            text: entry(true),
        },
        Tool::ClaudeCode => Line {
            place: match shell {
                Shell::Sh => "Run in a terminal".into(),
                Shell::PowerShell => "Run in PowerShell".into(),
            },
            // PowerShell drops a bare `--` before some programs: quoted, it is a word.
            text: match shell {
                Shell::Sh => format!("claude mcp add rbs -- {command}"),
                Shell::PowerShell => format!("claude mcp add rbs '--' {command}"),
            },
        },
        Tool::Other => Line {
            place: "The command that starts the MCP server (stdio)".into(),
            text: command,
        },
    }
}

/// `rbs-mcp` beside `exe` (the app's own executable), or why no line can be made:
/// missing, or RBS runs from a place other programs cannot start it from (an AppImage
/// mount, a Flatpak or Snap sandbox).
pub fn locate_mcp(exe: &Path, sandboxed: Option<&str>) -> Result<PathBuf, String> {
    if let Some(kind) = sandboxed {
        return Err(format!(
            "RBS runs as {kind}, so AI tools cannot start rbs-mcp from it. Install RBS from the download on GitHub to connect AI tools."
        ));
    }
    let name = if cfg!(windows) {
        "rbs-mcp.exe"
    } else {
        "rbs-mcp"
    };
    let path = exe.with_file_name(name);
    if path.is_file() {
        Ok(path)
    } else {
        Err(format!(
            "rbs-mcp is not next to RBS ({}). Reinstall RBS to connect AI tools.",
            path.display()
        ))
    }
}

/// The package that sandboxes RBS, from the environment: AppImage, Flatpak or Snap.
pub fn sandbox_from_env(var: impl Fn(&str) -> Option<String>) -> Option<&'static str> {
    if var("APPIMAGE").is_some() {
        Some("an AppImage")
    } else if var("FLATPAK_ID").is_some() {
        Some("a Flatpak")
    } else if var("SNAP").is_some() {
        Some("a Snap")
    } else {
        None
    }
}

/// Longest name shown for a connected AI tool, in characters.
pub const MAX_CLIENT_NAME: usize = 64;

/// The name a connected AI tool reports (`session.hello` `client.name`), safe to show:
/// control, line break and bidirectional characters removed, cut to
/// [`MAX_CLIENT_NAME`] characters. It is untrusted: shown after "Reported as", never
/// used as a caller label (decision 7).
pub fn clean_client_name(name: &str) -> String {
    name.chars()
        .filter(|c| !c.is_control() && !is_bidi(*c) && !matches!(c, '\u{2028}' | '\u{2029}'))
        .take(MAX_CLIENT_NAME)
        .collect::<String>()
        .trim()
        .to_owned()
}

fn is_bidi(c: char) -> bool {
    matches!(
        c,
        '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
    )
}

/// How many recent actions the page keeps.
pub const RING: usize = 20;

/// One recent action of the AI tools token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Activity {
    /// Seconds since the Unix epoch.
    pub at: u64,
    /// The method.
    pub method: &'static str,
    /// `None`: allowed. Otherwise the refusal's reason (`irreversible`, `on_air`, ...).
    pub refused: Option<&'static str>,
}

/// What the page reads: recent actions and connected AI tools.
#[derive(Debug, Default)]
pub struct Seen {
    /// Newest last, at most [`RING`].
    pub actions: VecDeque<Activity>,
    /// Connected AI tools sessions and the names they reported.
    pub clients: Vec<(u64, String)>,
}

/// The audit forward (decision 7). The audit sink the server gets is fixed when it
/// starts; it passes every record here. With no ring (AI tools never started this
/// Settings session) a record costs one lock and a check; started, the AI tools
/// token's `live` and `destructive` calls and refusals go into the ring.
#[derive(Default)]
pub struct Forward {
    ring: Mutex<Option<Seen>>,
    /// Says the page has something new to read; set by the app.
    changed: Mutex<Option<Box<dyn Fn() + Send>>>,
}

impl std::fmt::Debug for Forward {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Forward")
    }
}

impl Forward {
    /// A new forward with no ring.
    pub fn new() -> Arc<Forward> {
        Arc::new(Forward::default())
    }

    /// Calls `f` when there is something new (it must not block).
    pub fn notify_with(&self, f: impl Fn() + Send + 'static) {
        *self.changed.lock().unwrap_or_else(PoisonError::into_inner) = Some(Box::new(f));
    }

    /// Start: a new, empty ring.
    pub fn start(&self) {
        *self.ring.lock().unwrap_or_else(PoisonError::into_inner) = Some(Seen::default());
    }

    /// Stop: the ring is kept with no new records, until [`Forward::clear`]; nobody
    /// is connected any more.
    pub fn stop(&self) {
        let mut ring = self.ring.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(seen) = ring.as_mut() {
            seen.clients.clear();
        }
    }

    /// The Settings window closed while stopped: the ring goes.
    pub fn clear(&self) {
        *self.ring.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }

    /// The actions (newest first) and the connected names.
    pub fn read(&self) -> (Vec<Activity>, Vec<String>) {
        let ring = self.ring.lock().unwrap_or_else(PoisonError::into_inner);
        match ring.as_ref() {
            Some(seen) => (
                seen.actions.iter().rev().cloned().collect(),
                seen.clients.iter().map(|(_, n)| n.clone()).collect(),
            ),
            None => (Vec::new(), Vec::new()),
        }
    }

    /// One audit record, on the API thread, at `now` (seconds since the epoch).
    /// `started` is read when the record arrives, so after Stop nothing is added.
    pub fn record(&self, event: &AuditEvent, now: u64, started: bool) {
        if !started {
            return;
        }
        let mut ring = self.ring.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(seen) = ring.as_mut() else {
            return;
        };
        let changed = match event {
            AuditEvent::Irreversible {
                kind: Some(TokenKind::AiTools),
                method,
                ..
            } => push(seen, now, method, None),
            AuditEvent::NotPermitted {
                kind: Some(TokenKind::AiTools),
                method,
                reason,
                ..
            } => push(seen, now, method, Some(reason)),
            AuditEvent::Authenticated {
                conn,
                kind: Some(TokenKind::AiTools),
                client,
                ..
            } => {
                seen.clients.push((
                    conn.get(),
                    clean_client_name(client.as_deref().unwrap_or("")),
                ));
                true
            }
            AuditEvent::Closed { conn, .. } | AuditEvent::Disconnected { conn } => {
                let before = seen.clients.len();
                seen.clients.retain(|(c, _)| *c != conn.get());
                seen.clients.len() != before
            }
            _ => false,
        };
        drop(ring);
        if changed
            && let Some(f) = self
                .changed
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .as_ref()
        {
            f();
        }
    }
}

fn push(seen: &mut Seen, at: u64, method: &'static str, refused: Option<&'static str>) -> bool {
    if seen.actions.len() == RING {
        seen.actions.pop_front();
    }
    seen.actions.push_back(Activity {
        at,
        method,
        refused,
    });
    true
}

/// Why a refusal happened, for the page.
pub fn refusal_text(reason: &str) -> &'static str {
    match reason {
        "on_air" => "It would change what is on air.",
        "network" => "It could reach another computer.",
        "scopes" => "It asked for more than this token holds.",
        _ => "It cannot be undone, and Allow actions that cannot be undone is off.",
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
mod tests;
