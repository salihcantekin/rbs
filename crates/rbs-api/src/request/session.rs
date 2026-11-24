//! Control API handshake requests.

use serde::{Deserialize, Serialize};

/// `session.hello`: the protocol versions the client speaks, and who it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Hello {
    /// `major.minor` versions, for example `["1.0"]`.
    pub protocols: Vec<String>,
    /// The client.
    pub client: ClientInfo,
}

/// A client's or server's name and version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ClientInfo {
    /// Name.
    pub name: String,
    /// Version.
    pub version: String,
}

impl ClientInfo {
    /// A name and version.
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
        }
    }
}

/// `session.authenticate`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Authenticate {
    /// The token's text.
    pub token: String,
}

impl std::fmt::Debug for Authenticate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Authenticate { token: <redacted> }")
    }
}
