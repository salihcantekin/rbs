//! `rbs-mcp`: MCP on stdin and stdout, one JSON-RPC message per line, for one RBS
//! control API token (mcp-server spec). Started by the AI tool; ends when stdin
//! closes. Options: `--token-file <path>` and `--tools <namespace,...>`. There is no
//! option for the token itself.

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use rbs_core::time::MonotonicClock;
use rbs_mcp::Mcp;
use rbs_mcp::link::IpcConnector;
use rbs_mcp::token::TokenSource;

const USAGE: &str = "usage: rbs-mcp [--token-file <path>] [--tools <namespace,...>]";

fn main() -> ExitCode {
    let mut token_file = None;
    let mut namespaces = None;
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--token-file") => match args.next() {
                Some(p) => token_file = Some(PathBuf::from(p)),
                None => return usage(),
            },
            Some("--tools") => match args.next().and_then(|t| t.into_string().ok()) {
                Some(list) => {
                    namespaces = Some(
                        list.split(',')
                            .map(|s| s.trim().to_owned())
                            .filter(|s| !s.is_empty())
                            .collect(),
                    );
                }
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
    let mut mcp = Mcp::new(connector, Arc::new(MonotonicClock::new()), namespaces);
    if let Err(why) = mcp.start() {
        eprintln!("rbs-mcp: {why}");
        return ExitCode::FAILURE;
    }
    if let Some(why) = mcp.hint_reason() {
        eprintln!(
            "rbs-mcp: not connected to RBS yet ({why}); waiting for Start in Settings, AI tools"
        );
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
        for out in outcome.out {
            if writeln!(stdout, "{out}")
                .and_then(|()| stdout.flush())
                .is_err()
            {
                return ExitCode::SUCCESS;
            }
        }
        if let Some(why) = outcome.exit {
            eprintln!("rbs-mcp: {why}");
            return ExitCode::FAILURE;
        }
    }
    ExitCode::SUCCESS
}

fn usage() -> ExitCode {
    eprintln!("{USAGE}");
    ExitCode::from(2)
}
