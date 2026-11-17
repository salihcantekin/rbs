//! AI tools as the app runs them (ADR-0134): with remote control off, Start serves only
//! IPC, `rbs-mcp` connects with the token RBS wrote and is bound by its access; Stop
//! closes its session and, with remote control off, removes the endpoint, so `rbs-mcp`
//! waits in the hint state; New token disconnects it and it reconnects by itself. No
//! studio: the handshake and `api.describe` are answered by the API thread. Waits end
//! on answers; timeouts are hang guards only (ADR-0032).

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rbs_app::remote_control::{RemoteOptions, RemoteServer, SlotFlags};
use rbs_core::time::TestClock;
use rbs_mcp::link::IpcConnector;
use rbs_mcp::token::TokenSource;
use rbs_mcp::{Mcp, RETRY_EVERY};
use rbs_remote_settings::ai_tools as ai_settings;
use rbs_remote_settings::{Access, RemoteProfile};
use serde_json::Value;

fn temp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("rbs-app-ai-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn endpoint(dir: &Path) -> rbs_api_ipc::Endpoint {
    if cfg!(windows) {
        rbs_api_ipc::Endpoint(format!(r"\\.\pipe\rbs-app-ai-test-{}", std::process::id()).into())
    } else {
        rbs_api_ipc::Endpoint(dir.join("ipc").join("c.sock"))
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
fn start_serves_only_ipc_and_stop_sends_rbs_mcp_back_to_the_hint() {
    let config = temp("start");
    let at = endpoint(&config);
    // Start: the token and its file.
    ai_settings::prepare_start(&config).unwrap();
    let remote = RemoteProfile::new();
    let flags = Arc::new(SlotFlags::default());
    flags.set(true, false);
    let ai = ai_settings::read(&config).unwrap();
    let (mut server, _endpoint) = RemoteServer::start_api(
        flags.slot(ai_settings::union_store(&remote, &ai)),
        RemoteOptions {
            control_waker: Box::new(|| {}),
            audit: Box::new(|_| {}),
            ipc: None,
            levels: None,
        },
    )
    .unwrap();
    server.start_ipc(at.clone()).unwrap();
    assert!(!server.serves_ws(), "no port while remote control is off");

    let clock = Arc::new(TestClock::new());
    let connector = IpcConnector {
        token: TokenSource::choose(Some(ai_settings::token_file(&config)), None, &config),
        endpoint: Some(at.clone()),
    };
    let mut mcp = Mcp::new(connector, clock.clone(), None);
    mcp.start().unwrap();
    mcp.handle(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    assert!(mcp.connected(), "{:?}", mcp.hint_reason());
    let names = tools(&mut mcp);
    assert!(
        names.contains(&"scene_add".to_owned()),
        "Control by default"
    );
    assert!(!names.contains(&"settings_set".to_owned()));
    assert!(
        !names.contains(&"record_start".to_owned()),
        "no permission by default"
    );

    // A tokenless IPC client gets nothing from hello while only AI tools run.
    let mut cli = rbs_api_ipc::IpcClient::connect(&at, std::time::Duration::from_secs(20)).unwrap();
    cli.send(r#"{"jsonrpc":"2.0","id":1,"method":"session.hello","params":{"protocols":["1.0"],"client":{"name":"rbs-cli","version":"1"}}}"#).unwrap();
    let Some(rbs_api_ipc::Received::Message(m)) =
        cli.recv(std::time::Duration::from_secs(20)).unwrap()
    else {
        panic!("no hello answer");
    };
    let hello: Value = serde_json::from_str(&m).unwrap();
    assert!(hello["result"].get("scopes").is_none(), "{hello}");

    // Full control: the session reconnects with what the token allows now.
    let mut ai = ai_settings::read(&config).unwrap();
    ai.set_access(Access::Full, true);
    ai_settings::save(&config, &ai).unwrap();
    server.set_slot(flags.slot(ai_settings::union_store(&remote, &ai)));
    let o = mcp
        .handle(r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"scene_list"}}"#);
    assert!(
        o.out.iter().any(|l| l.contains("list_changed")),
        "{:?}",
        o.out
    );
    clock.advance(RETRY_EVERY);
    let names = tools(&mut mcp);
    assert!(names.contains(&"record_start".to_owned()), "{names:?}");

    // New token: the old one stops working; rbs-mcp reads the new file.
    ai_settings::rotate(&config).unwrap();
    let ai = ai_settings::read(&config).unwrap();
    server.set_slot(flags.slot(ai_settings::union_store(&remote, &ai)));
    mcp.handle(r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"scene_list"}}"#);
    clock.advance(RETRY_EVERY);
    tools(&mut mcp);
    assert!(mcp.connected(), "{:?}", mcp.hint_reason());

    // Stop with remote control off: sessions close and the endpoint goes.
    flags.set(false, true);
    server.set_slot(flags.slot(ai_settings::union_store(&remote, &ai)));
    drop(server);
    if !cfg!(windows) {
        assert!(!at.0.exists(), "the socket file is removed");
    }
    mcp.handle(r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"scene_list"}}"#);
    clock.advance(RETRY_EVERY);
    assert_eq!(tools(&mut mcp), ["studio_summary"]);
    assert!(!mcp.connected());
    let _ = std::fs::remove_dir_all(&config);
}
