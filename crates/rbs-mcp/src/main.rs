//! `rbs-mcp`: MCP on stdin and stdout, one JSON-RPC message per line, for one RBS
//! control API token (mcp-server spec). Started by the AI tool; ends when stdin
//! closes. Options: `--token-file <path>` and `--tools <namespace,...>`. There is no
//! option for the token itself.

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use rbs_core::time::MonotonicClock;
use rbs_mcp::link::IpcConnector;
use rbs_mcp::token::TokenSource;
use rbs_mcp::{Mcp, ToolSet};

const USAGE: &str = "usage: rbs-mcp [--token-file <path>] [--tools <namespace,...|all>]";

fn main() -> ExitCode {
    let mut token_file = None;
    let mut tools = ToolSet::default();
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--token-file") => match args.next() {
                Some(p) => token_file = Some(PathBuf::from(p)),
                None => return usage(),
            },
            Some("--tools") => match args.next().and_then(|t| t.into_string().ok()) {
                Some(list) => tools = ToolSet::parse(&list),
                None => return usage(),
            },
            _ => return usage(),
        }
    }
    let token = TokenSource::choose(
        token_file,
        std::env::var("RBS_TOKEN").ok(),
        &rbs_remote_settings::config_dir(),
    );
    let connector = IpcConnector {
        token,
        endpoint: None,
    };
    let mut mcp = Mcp::new(connector, Arc::new(MonotonicClock::new()), tools);
    if let Err(why) = mcp.start() {
        note(format_args!("rbs-mcp: {why}"));
        return ExitCode::FAILURE;
    }
    let mut last_hint = mcp.hint_reason().map(str::to_owned);
    if let Some(why) = &last_hint {
        note(format_args!(
            "rbs-mcp: not connected to RBS yet ({why}); waiting for Start in Settings, AI tools"
        ));
    }
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else {
            break;
        };
        if line.trim().is_empty() {
            continue;
        }
        let outcome = mcp.handle(&line);
        let current_hint = mcp.hint_reason().map(str::to_owned);
        if current_hint != last_hint {
            if let Some(why) = &current_hint {
                note(format_args!("rbs-mcp: not connected to RBS ({why})"));
            }
            last_hint = current_hint;
        }
        for out in outcome.out {
            if writeln!(stdout, "{out}")
                .and_then(|()| stdout.flush())
                .is_err()
            {
                return ExitCode::SUCCESS;
            }
        }
        if let Some(why) = outcome.exit {
            note(format_args!("rbs-mcp: {why}"));
            return ExitCode::FAILURE;
        }
    }
    ExitCode::SUCCESS
}

fn usage() -> ExitCode {
    note(format_args!("{USAGE}"));
    ExitCode::from(2)
}

/// A line to stderr. `eprintln!` would panic when the AI tool closed stderr; the line is
/// dropped instead (crash-resilience spec, "Standard output and error").
fn note(line: std::fmt::Arguments<'_>) {
    let _ = writeln!(std::io::stderr().lock(), "{line}");
}
