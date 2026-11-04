//! A small control API client over local IPC (ADR-0044, control API spec "Local IPC"):
//! `rbs-cli token` sends its changes to a running RBS this way, so they take effect at
//! once. The OS identifies this program as the same user, so `session.hello` is all
//! the authentication it needs.

use std::time::Duration;

use rbs_api_ipc::{ClientError, Endpoint, IpcClient, Received};
use serde_json::{Value, json};

/// How long to wait for the connection and for each answer. The app answers from its
/// control thread between frames, normally within milliseconds; this bounds a hung one.
const WAIT: Duration = Duration::from_secs(10);

/// The outcome of trying to reach a running RBS.
pub enum Reach {
    /// It serves the control API to this user and allows token management.
    Connected(RunningApp),
    /// Nothing serves the endpoint: RBS is not running, or remote control is off.
    NotRunning,
    /// Something serves it but could not be used, in words.
    Unusable(String),
}

/// A connection to a running RBS, after the handshake.
pub struct RunningApp {
    client: IpcClient,
    next: i64,
}

/// A request that failed: the error's stable kind (`not_found`, `conflict`, ...) when
/// RBS answered with one, and its message.
#[derive(Debug)]
pub struct CallError {
    pub kind: Option<String>,
    pub message: String,
}

impl RunningApp {
    /// Connects to `endpoint` and says hello.
    pub fn connect(endpoint: &Endpoint) -> Reach {
        let client = match IpcClient::connect(endpoint, WAIT) {
            Ok(c) => c,
            Err(ClientError::NotRunning(_)) => return Reach::NotRunning,
            Err(e) => return Reach::Unusable(e.to_string()),
        };
        let mut app = RunningApp { client, next: 1 };
        let hello = app.call(
            "session.hello",
            json!({
                "protocols": ["1.0"],
                "client": {"name": "rbs-cli", "version": env!("CARGO_PKG_VERSION")},
            }),
        );
        match hello {
            Ok(r)
                if r["scopes"]
                    .as_array()
                    .is_some_and(|s| s.iter().any(|x| x == "tokens")) =>
            {
                Reach::Connected(app)
            }
            Ok(_) => Reach::Unusable("it does not allow token management here".into()),
            Err(e) => Reach::Unusable(e.message),
        }
    }

    /// Calls `method` and returns its result.
    pub fn call(&mut self, method: &str, params: Value) -> Result<Value, CallError> {
        let id = self.next;
        self.next += 1;
        let text = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let gone = |message: String| CallError {
            kind: None,
            message,
        };
        self.client
            .send(&text.to_string())
            .map_err(|e| gone(format!("RBS could not be sent the request ({e})")))?;
        loop {
            match self.client.recv(WAIT) {
                Ok(Some(Received::Message(m))) => {
                    let Ok(v) = serde_json::from_str::<Value>(&m) else {
                        continue;
                    };
                    // Events and other answers are not for this call.
                    if v["id"] != id {
                        continue;
                    }
                    if let Some(e) = v.get("error") {
                        return Err(CallError {
                            kind: e["data"]["kind"].as_str().map(str::to_owned),
                            message: e["message"].as_str().unwrap_or("failed").to_owned(),
                        });
                    }
                    return Ok(v["result"].clone());
                }
                Ok(Some(Received::Closed(r))) => {
                    return Err(gone(format!("RBS closed the connection ({r:?})")));
                }
                Ok(None) => {
                    return Err(gone(format!(
                        "RBS did not answer within {} seconds",
                        WAIT.as_secs()
                    )));
                }
                Err(e) => return Err(gone(format!("the connection to RBS failed ({e})"))),
            }
        }
    }
}

impl Drop for RunningApp {
    fn drop(&mut self) {
        self.client.close();
    }
}
