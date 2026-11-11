//! MCP over stdio for one RBS control API token (ADR-0052, mcp-server spec).
//!
//! This is the first slice: while RBS cannot be reached, the only tool is
//! `studio_summary`, and it says how to turn remote control on. Generated tools,
//! the IPC session and the token file come next. Nothing here talks to the engine
//! except through that future control API client.

use serde_json::{Value, json};

/// Why RBS is not in use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Link {
    /// Local IPC did not answer. Remote control is off, or RBS is not running.
    Unreachable,
}

/// One MCP JSON-RPC message in, and the response to write, if the message expects one.
pub fn handle(link: Link, message: &str) -> Option<String> {
    let value: Value = serde_json::from_str(message).ok()?;
    let id = value.get("id").cloned();
    let method = value.get("method").and_then(Value::as_str).unwrap_or("");
    let result = match method {
        "initialize" => json!({
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": { "listChanged": true } },
            "serverInfo": { "name": "rbs", "version": env!("CARGO_PKG_VERSION") },
            "instructions": "RBS is a live production engine. Read studio_summary first. Edits are undoable. Live actions are not."
        }),
        "tools/list" => json!({ "tools": tools(link) }),
        "tools/call" => tool_call(link, value.get("params").unwrap_or(&Value::Null)),
        "notifications/initialized" => return None,
        _ => {
            return id.map(|id| {
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32601, "message": "method not found" }
                })
                .to_string()
            });
        }
    };
    id.map(|id| {
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result
        })
        .to_string()
    })
}

fn tools(link: Link) -> Vec<Value> {
    match link {
        Link::Unreachable => vec![json!({
            "name": "studio_summary",
            "description": "What the studio is doing. Remote control is off, so this is the only tool.",
            "inputSchema": { "type": "object", "properties": {} }
        })],
    }
}

fn tool_call(link: Link, params: &Value) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    if link == Link::Unreachable && name == "studio_summary" {
        let text = "Remote control is off, so RBS is not accepting apps. Turn it on in Settings, Remote control, then try again.";
        return json!({
            "content": [{ "type": "text", "text": text }],
            "isError": false
        });
    }
    json!({
        "content": [{ "type": "text", "text": "that tool is not available" }],
        "isError": true
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn an_unreachable_studio_lists_only_studio_summary() {
        let list = handle(
            Link::Unreachable,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
        )
        .unwrap();
        let value: Value = serde_json::from_str(&list).unwrap();
        let tools = value["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "studio_summary");
        let call = handle(
            Link::Unreachable,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"studio_summary"}}"#,
        )
        .unwrap();
        let value: Value = serde_json::from_str(&call).unwrap();
        let text = value["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("Remote control"));
        assert!(!text.contains("RBS_TOKEN"));
    }
}
