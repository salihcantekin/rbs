//! `rbs-mcp` against a real control API server over real IPC (mcp-server spec,
//! conformance): the token file's token narrows the session, the tools follow its
//! scopes, a stopped server sends it back to the hint state, and a rotated file is
//! picked up without a restart. No studio: the handshake and `api.describe` are
//! answered by the API thread. Waits end on answers; timeouts are hang guards only.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rbs_api::request::ClientInfo;
use rbs_api::{
    ApiOptions, ApiServer, ControlEndpoint, ControlTransport, Scope, ScopeSet, ServerConfig, Token,
    TokenEntry, TokenStore,
};
use rbs_api_ipc::{Endpoint, IpcConfig, IpcTransport};
use rbs_core::time::TestClock;
use rbs_mcp::link::IpcConnector;
use rbs_mcp::token::TokenSource;
use rbs_mcp::{HINT, Mcp, RETRY_EVERY, ToolSet};
use serde_json::Value;

fn temp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("rbs-mcp-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn endpoint(dir: &Path, name: &str) -> Endpoint {
    if cfg!(windows) {
        Endpoint(format!(r"\\.\pipe\rbs-mcp-test-{name}-{}", std::process::id()).into())
    } else {
        Endpoint(dir.join("ipc").join("c.sock"))
    }
}

fn write_token(path: &Path, token: &Token) {
    std::fs::write(path, token.expose()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}

fn serve(tokens: TokenStore, at: &Endpoint) -> (ApiServer, ControlEndpoint, IpcTransport) {
    let config = ServerConfig::new(ClientInfo::new("RBS", "test")).unwrap();
    let (api, control) = ApiServer::start(config, tokens, ApiOptions::new(|| {})).unwrap();
    let mut ipc = IpcTransport::new(IpcConfig::new(at.clone()));
    ipc.start(api.inbox()).unwrap();
    (api, control, ipc)
}

fn entry(name: &str, token: &Token, scopes: ScopeSet, irreversible: bool) -> TokenEntry {
    TokenEntry {
        kind: rbs_api::TokenKind::User,
        irreversible,
        name: name.into(),
        scopes,
        hash: token.hash(),
    }
}

fn tools(mcp: &mut Mcp<IpcConnector>) -> Vec<String> {
    let o = mcp.handle(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
    let v: Value = serde_json::from_str(o.out.last().unwrap()).unwrap();
    v["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn the_token_file_narrows_the_session_and_rotation_needs_no_restart() {
    let dir = temp("rotate");
    let at = endpoint(&dir, "rotate");
    let file = dir.join("agent-token");
    let view = Token::generate().unwrap();
    let control = Token::generate().unwrap();
    let mut store = TokenStore::new();
    store.insert(entry(
        "View",
        &view,
        ScopeSet::EMPTY.with(Scope::Read),
        true,
    ));
    store.insert(entry(
        "AI",
        &control,
        ScopeSet::EMPTY.with(Scope::Read).with(Scope::Control),
        false,
    ));

    // No file yet: the hint.
    let clock = Arc::new(TestClock::new());
    let connector = IpcConnector {
        token: TokenSource::choose(Some(file.clone()), None, &dir),
        endpoint: Some(at.clone()),
    };
    let mut mcp = Mcp::new(connector, clock.clone(), ToolSet::All);
    let (api, _control, ipc) = serve(store, &at);
    mcp.start().unwrap();
    mcp.handle(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    assert!(!mcp.connected());
    assert_eq!(mcp.hint_reason(), Some("the token file is missing"));
    let o = mcp.handle(
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"studio_summary"}}"#,
    );
    assert!(o.out[0].contains(HINT), "{:?}", o.out);

    // View only.
    write_token(&file, &view);
    clock.advance(RETRY_EVERY);
    let names = tools(&mut mcp);
    assert!(mcp.connected(), "{:?}", mcp.hint_reason());
    assert!(names.contains(&"scene_list".to_owned()));
    assert!(!names.contains(&"scene_add".to_owned()));

    // The server stops: back to the hint at the next call.
    drop(ipc);
    let o = mcp
        .handle(r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"scene_list"}}"#);
    assert!(!mcp.connected());
    assert!(o.out.iter().any(|l| l.contains("list_changed")));
    drop(api);

    // A rotated file and a new server: the next attempt uses the new token.
    let mut store = TokenStore::new();
    store.insert(entry(
        "AI",
        &control,
        ScopeSet::EMPTY.with(Scope::Read).with(Scope::Control),
        false,
    ));
    let (_api, _control, _ipc) = serve(store, &at);
    write_token(&file, &control);
    clock.advance(RETRY_EVERY);
    let names = tools(&mut mcp);
    assert!(mcp.connected(), "{:?}", mcp.hint_reason());
    assert!(names.contains(&"scene_add".to_owned()));
    let response = mcp.handle(r#"{"jsonrpc":"2.0","id":4,"method":"tools/list"}"#);
    let list: Value = serde_json::from_str(response.out.last().unwrap()).unwrap();
    let catalog = rbs_api::schema::builtin_schemas().unwrap();
    for tool in list["result"]["tools"].as_array().unwrap() {
        let name = tool["name"].as_str().unwrap();
        if name == "studio_summary" {
            continue;
        }
        let method = catalog
            .iter()
            .find(|(method, _)| method.replace('.', "_") == name)
            .unwrap_or_else(|| panic!("unexpected tool {name}"));
        assert_eq!(tool["inputSchema"], method.1.input_schema, "{name}");
        if method.1.output_schema["type"] == "object" {
            assert_eq!(tool["outputSchema"], method.1.output_schema, "{name}");
        } else if name == "source_attributes_clipboard" {
            assert!(jsonschema::is_valid(
                &tool["outputSchema"],
                &serde_json::json!({"result": null})
            ));
            assert!(jsonschema::is_valid(
                &tool["outputSchema"],
                &serde_json::json!({"source": 1, "name": "Camera", "type": "camera", "groups": []})
            ));
        } else {
            assert!(tool.get("outputSchema").is_none(), "{name}");
        }
    }
    assert!(!names.contains(&"source_set_sync_offset".to_owned()));
    assert!(
        !names.contains(&"record_start".to_owned()),
        "no live tools without the permission"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_wrong_token_waits_in_the_hint_state() {
    let dir = temp("wrong");
    let at = endpoint(&dir, "wrong");
    let file = dir.join("agent-token");
    write_token(&file, &Token::generate().unwrap());
    let (_api, _control, _ipc) = serve(TokenStore::new(), &at);
    let connector = IpcConnector {
        token: TokenSource::File(file),
        endpoint: Some(at),
    };
    let mut mcp = Mcp::new(connector, Arc::new(TestClock::new()), ToolSet::All);
    mcp.start().unwrap();
    assert_eq!(mcp.hint_reason(), Some("RBS did not accept the token"));
    assert_eq!(tools(&mut mcp), ["studio_summary"]);
    let _ = std::fs::remove_dir_all(&dir);
}
