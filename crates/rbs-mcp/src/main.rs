//! `rbs-mcp`: MCP on stdin, one JSON-RPC message per line.
//!
//! The IPC session is not connected yet. Until it is, every request is answered as
//! an unreachable studio (mcp-server spec: only `studio_summary`).

use std::io::{BufRead, Write};

fn main() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else {
            break;
        };
        if let Some(response) = rbs_mcp::handle(rbs_mcp::Link::Unreachable, &line) {
            let _ = writeln!(stdout, "{response}");
            let _ = stdout.flush();
        }
    }
}
