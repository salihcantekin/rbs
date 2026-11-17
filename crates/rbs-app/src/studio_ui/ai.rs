//! Settings, AI tools (ADR-0134): Start and Stop, "Start when RBS opens", the AI tools
//! token's access and permission, New token, the connection line and recent actions.
//! The server side is `remote_control` (the API thread with optional transports) and
//! `rbs_remote_settings::ai_tools` (`ai-tools.toml`, `agent-token`); `rbs-mcp` is the
//! program the AI tool starts.

use std::path::PathBuf;

use rbs_app::ai_tools::{self, Shell, Tool};
use rbs_remote_settings::Access;
use rbs_remote_settings::ai_tools as ai_settings;
use slint::{ModelRc, VecModel};

use super::{StudioUi, log, strings};
use crate::ui::{AiActionRow, Studio};

/// Seconds since the Unix epoch.
pub(super) fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// "14:03:22", local time.
fn time_text(secs: u64) -> String {
    let at = std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs);
    chrono::DateTime::<chrono::Local>::from(at)
        .format("%H:%M:%S")
        .to_string()
}

impl StudioUi {
    /// "Start": makes sure the token and its file exist, then serves IPC for AI tools
    /// (ADR-0134 decision 2). Fails as a whole, with the reason on the page, when IPC
    /// cannot be served or the settings cannot be written.
    pub fn ai_start(&mut self) {
        if self.ai_started {
            return;
        }
        self.ai_note.clear();
        if self.host.is_none() {
            self.ai_failure =
                Some("AI tools start once the studio runs. Try again in a moment.".into());
            return;
        }
        let lock = match rbs_remote_settings::try_lock(&self.config) {
            Ok(Some(lock)) => Some(lock),
            Ok(None) => {
                self.ai_failure =
                    Some("Another program (rbs-cli) is changing the settings. Try again.".into());
                return;
            }
            Err(e) => {
                log(&format!("AI tools: settings not locked: {e}"));
                None
            }
        };
        let prepared = ai_settings::prepare_start(&self.config);
        let read = ai_settings::read(&self.config);
        drop(lock);
        let prepared = match prepared.and_then(|p| read.map(|ai| (p, ai))) {
            Ok((p, ai)) => {
                self.ai = ai;
                p
            }
            Err(why) => {
                log(&format!("AI tools did not start: {why}"));
                self.ai_failure = Some(format!("AI tools did not start: {why}"));
                return;
            }
        };
        if let Some(name) = &prepared.renamed {
            self.ai_note = format!(
                "Your access token named \"AI tools\" is now named \"{name}\". Its app reconnects by itself."
            );
        } else if prepared.other_kind {
            self.ai_note = "The token file held a remote control token. RBS put an AI tools token there instead; the remote control token still works.".into();
        }
        // A renamed user token is in remote-control.toml now.
        self.reload_tokens();
        self.ai_started = true;
        if self.remote_server.is_none() && !self.start_api() {
            self.ai_started = false;
            self.ai_failure = self
                .remote_failure
                .take()
                .or(Some("AI tools did not start.".into()));
            return;
        }
        if let Some(why) = self.serve_ipc() {
            log(&format!("AI tools did not start: {why}"));
            self.ai_started = false;
            self.ai_failure = Some(format!("AI tools did not start: {why}"));
            if !self.remote.enabled {
                self.stop_remote();
            }
            return;
        }
        self.ai_failure = None;
        self.ai_forward.start();
        self.sync_remote();
        log("AI tools started");
    }

    /// Serves IPC if the server does not yet; the reason when it cannot.
    fn serve_ipc(&mut self) -> Option<String> {
        let server = self.remote_server.as_mut()?;
        if server.ipc_endpoint().is_some() {
            return None;
        }
        let endpoint = match rbs_api_ipc::Endpoint::for_current_user() {
            Ok(e) => e,
            Err(why) => return Some(why),
        };
        let shown = endpoint.to_string();
        match server.start_ipc(endpoint) {
            Ok(()) => {
                log(&format!("AI tools: local IPC at {shown}"));
                None
            }
            Err(why) => Some(why),
        }
    }

    /// "Stop": the AI tools token's sessions are closed; with remote control off the
    /// API thread and IPC stop too and the socket is removed.
    pub fn ai_stop(&mut self) {
        if !self.ai_started {
            return;
        }
        self.ai_started = false;
        self.ai_failure = None;
        self.ai_forward.stop();
        self.push_slot();
        self.sync_remote();
        log("AI tools stopped");
    }

    /// AI tools stopped by something else than Stop; `why` shows on the page.
    pub(super) fn ai_stopped_by(&mut self, why: &str) {
        self.ai_started = false;
        self.ai_failure = Some(why.to_owned());
        self.ai_forward.stop();
        self.push_slot();
        log(&format!("AI tools: {why}"));
    }

    /// The studio runs: start AI tools when "Start when RBS opens" is on.
    pub fn ai_autostart(&mut self) {
        if self.ai.start_on_open && !self.ai_started {
            self.ai_start();
        }
    }

    /// Reads `ai-tools.toml` again (after `tokens.revoke` of the AI tools token).
    pub(super) fn read_ai(&mut self) {
        match ai_settings::read(&self.config) {
            Ok(ai) => {
                if ai != self.ai {
                    self.ai = ai;
                    self.push_slot();
                }
            }
            Err(why) => log(&format!("AI tools settings not read again: {why}")),
        }
    }

    /// Changes `ai-tools.toml` under the settings lock and gives the server the new
    /// tokens.
    fn change_ai(
        &mut self,
        f: impl FnOnce(&std::path::Path, &mut ai_settings::AiToolsSettings) -> Result<bool, String>,
    ) {
        let lock = match rbs_remote_settings::try_lock(&self.config) {
            Ok(Some(lock)) => Some(lock),
            Ok(None) => {
                self.ai_note =
                    "Another program (rbs-cli) is changing the settings. Try again.".into();
                return;
            }
            Err(e) => {
                log(&format!("AI tools: settings not locked: {e}"));
                None
            }
        };
        let mut ai = match ai_settings::read(&self.config) {
            Ok(ai) => ai,
            Err(why) => {
                self.ai_note = format!("The AI tools settings cannot be used: {why}");
                return;
            }
        };
        match f(&self.config, &mut ai) {
            Ok(true) => {
                if let Err(e) = ai_settings::save(&self.config, &ai) {
                    self.ai_note = format!("The AI tools settings could not be saved: {e}");
                    return;
                }
            }
            Ok(false) => {}
            Err(why) => {
                self.ai_note = why;
                return;
            }
        }
        // Rotation saves on its own; read what is saved now.
        self.ai = ai_settings::read(&self.config).unwrap_or(ai);
        drop(lock);
        self.ai_note.clear();
        self.push_slot();
    }

    pub fn set_ai_autostart(&mut self, on: bool) {
        self.change_ai(|_, ai| {
            let changed = ai.start_on_open != on;
            ai.start_on_open = on;
            Ok(changed)
        });
    }

    /// 0 View only, 1 Control, 2 Full control. The token's sessions reconnect with
    /// what it allows now.
    pub fn set_ai_access(&mut self, index: i32) {
        let access = match index {
            0 => Access::View,
            2 => Access::Full,
            _ => Access::Control,
        };
        self.change_ai(|_, ai| {
            let irreversible = ai.irreversible;
            Ok(ai.set_access(access, irreversible))
        });
    }

    pub fn set_ai_irreversible(&mut self, on: bool) {
        self.change_ai(|_, ai| {
            let access = ai.access();
            Ok(ai.set_access(access, on))
        });
    }

    /// "New token": the old token stops working; `rbs-mcp` reads the new file on its
    /// next connection.
    pub fn ai_new_token(&mut self) {
        self.change_ai(|config, _| ai_settings::rotate(config).map(|()| false));
        log("AI tools: made a new token");
    }

    pub fn pick_ai_tool(&mut self, index: i32) {
        self.ai_tool = Tool::at(index);
    }

    /// The picked AI tool's configuration file, from this user's environment.
    fn ai_config_file(&self) -> Option<PathBuf> {
        ai_tools::config_file(self.ai_tool, ai_tools::Os::native(), |v| {
            std::env::var(v).ok()
        })
    }

    /// "Show in folder" / "Open folder": the picked AI tool's configuration file, or
    /// its folder while the file does not exist. RBS never writes it (ADR-0134
    /// decision 5).
    pub fn ai_show_config(&self) {
        if let Some(file) = self.ai_config_file() {
            crate::reveal::show_in_folder(&file);
        }
    }

    /// The status line.
    fn ai_status(&self) -> (i32, String) {
        if self.ai_started {
            if self.ai.tokens.is_empty() {
                return (
                    1,
                    "The AI tools token was revoked. Press New token to connect AI tools again."
                        .into(),
                );
            }
            let n = self.ai_forward.read().1.len();
            let line = match n {
                0 => "Started. No AI tool is connected yet.".to_owned(),
                1 => "Started. 1 AI tool connected.".to_owned(),
                n => format!("Started. {n} AI tools connected."),
            };
            return (1, join(&line, &self.ai_note));
        }
        if let Some(why) = &self.ai_failure {
            return (2, join(why, &self.ai_note));
        }
        (
            0,
            join(
                "Stopped. AI tools cannot use RBS. Stopping is not a lock against other programs of your user account.",
                &self.ai_note,
            ),
        )
    }

    /// The connection line, or why there is none.
    fn ai_line(&self) -> Result<ai_tools::Line, String> {
        let exe = std::env::current_exe().map_err(|e| format!("RBS cannot find itself ({e})."))?;
        let sandbox = ai_tools::sandbox_from_env(|v| std::env::var(v).ok());
        let mcp = ai_tools::locate_mcp(&exe, sandbox)?;
        let token: PathBuf = ai_settings::token_file(&self.config);
        let token = std::path::absolute(&token).unwrap_or(token);
        Ok(ai_tools::line(self.ai_tool, &mcp, &token, Shell::native()))
    }

    pub(super) fn push_ai(&self, g: &Studio<'_>) {
        g.set_ai_ready(crate::AI_TOOLS_READY);
        let (state, status) = self.ai_status();
        g.set_ai_state(state);
        g.set_ai_status(status.into());
        let (actions, clients) = self.ai_forward.read();
        g.set_ai_clients(strings(clients));
        g.set_ai_autostart(self.ai.start_on_open);
        g.set_ai_access(match self.ai.access() {
            Access::View => 0,
            Access::Control => 1,
            Access::Full => 2,
        });
        g.set_ai_irreversible(self.ai.irreversible);
        g.set_ai_tool_names(strings(
            ai_tools::TOOLS.iter().map(|t| t.label().to_owned()),
        ));
        g.set_ai_tool(
            ai_tools::TOOLS
                .iter()
                .position(|t| *t == self.ai_tool)
                .unwrap_or(0) as i32,
        );
        match self.ai_line() {
            Ok(line) => {
                g.set_ai_where(line.place.into());
                g.set_ai_line_help(line.help.into());
                g.set_ai_line(line.text.into());
                g.set_ai_line_note("".into());
            }
            Err(why) => {
                g.set_ai_where("".into());
                g.set_ai_line_help("".into());
                g.set_ai_line("".into());
                g.set_ai_line_note(why.into());
            }
        }
        let config = self.ai_config_file();
        let (name, folder) = config
            .as_deref()
            .map(|f| {
                (
                    f.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    f.parent()
                        .map(|d| d.display().to_string())
                        .unwrap_or_default(),
                )
            })
            .unwrap_or_default();
        g.set_ai_config_name(name.into());
        g.set_ai_config_folder(folder.into());
        g.set_ai_config_exists(config.is_some_and(|f| f.is_file()));
        let rows: Vec<AiActionRow> = actions
            .iter()
            .map(|a| AiActionRow {
                time: time_text(a.at).into(),
                method: a.method.into(),
                allowed: a.refused.is_none(),
                why: a.refused.map(ai_tools::refusal_text).unwrap_or("").into(),
            })
            .collect();
        g.set_ai_actions(ModelRc::new(VecModel::from(rows)));
    }
}

fn join(a: &str, b: &str) -> String {
    if b.is_empty() {
        a.to_owned()
    } else {
        format!("{a} {b}")
    }
}
