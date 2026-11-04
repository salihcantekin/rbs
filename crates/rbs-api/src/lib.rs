//! The RBS control API (ADR-0044, `docs/specs/control-api.md`): the single way to
//! control and observe the engine, used by the desktop UI, remote clients, `rbs-cli`,
//! Stream Deck and other panels, scripts and tests.
//!
//! - [`request`], [`event`], [`types`], [`error`]: the typed requests, responses,
//!   events and errors of protocol version 1. The Rust types are the schema.
//! - [`codec`]: JSON-RPC 2.0 behind the [`Codec`] trait.
//! - [`token`], [`scope`]: bearer tokens stored as SHA-256, and scopes.
//! - [`session`]: the pure session state machine ([`Server`]).
//! - [`control`]: the control-thread side: [`ControlTarget`], [`ControlEndpoint`],
//!   [`EventPublisher`].
//! - [`transport`]: the [`ControlTransport`] port and the bounded [`TransportInbox`].
//! - [`thread`]: the API thread ([`ApiServer`]), which exists only while remote control
//!   is on.
//!
//! No engine, adapter, network, IPC, async-runtime or UI crate is a dependency
//! (principle P1). When remote control is off nothing here runs (ADR-0013).

pub mod codec;
pub mod control;
pub mod error;
pub mod event;
pub mod exposure;
pub mod request;
pub mod scope;
pub mod session;
pub mod thread;
pub mod token;
pub mod transport;
pub mod types;
pub mod wake;

pub use codec::{Codec, JsonCodec, RequestId};
pub use control::{ControlEndpoint, ControlTarget, EventLink, EventPublisher, Forwarded, Reply};
pub use error::{ApiError, ErrorKind};
pub use event::{Event, EventPayload, StateKey, Topic, TopicSet};
pub use exposure::{UpgradePolicy, UpgradeRefusal};
pub use request::{Command, Request, Response};
pub use scope::{Scope, ScopeSet};
pub use session::{
    Action, AuditEvent, Input, Limits, PROTOCOL_VERSION, Sampler, Server, ServerConfig,
};
pub use thread::{ApiOptions, ApiServer};
pub use token::{Token, TokenEntry, TokenHash, TokenStore};
pub use transport::{
    CloseReason, ConnectionHandle, ConnectionId, ConnectionSender, ControlTransport, MessageLimit,
    PeerInfo, ReadState, SendError, TransportEvent, TransportInbox, TransportKind,
};
