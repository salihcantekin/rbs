//! Requests and responses of protocol version 1 (first slice, control API spec "Initial
//! method set"). [`Request`] is everything a client may send; [`Command`] is the part
//! the session forwards to the control thread's [`ControlTarget`](crate::ControlTarget).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::event::Topic;
use crate::scope::{Scope, ScopeSet};
use crate::types::ItemStyle;
use crate::types::{
    Canvas, CaptureTarget, OutputProfile, RecordState, RecordingProfile, SeqMap, Settings,
    Snapshot, SourceInfo, SourceKind, StreamSettingsPatch, StreamState, StudioInfo,
};

/// Parameters of a method that takes none. Unknown fields are ignored.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Empty {}

/// `session.hello`: the protocol versions the client speaks, and who it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// `major.minor` versions, for example `["1.0"]`.
    pub protocols: Vec<String>,
    /// The client.
    pub client: ClientInfo,
}

/// A client's or server's name and version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
pub struct Authenticate {
    /// The token's text.
    pub token: String,
}

impl std::fmt::Debug for Authenticate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Authenticate { token: <redacted> }")
    }
}

/// `source.targets`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetsParams {
    /// The kind whose capture targets to list.
    pub kind: SourceKind,
}

/// `source.add`: a source of `kind` capturing one target, on top of the program scene.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddSource {
    /// Its kind.
    pub kind: SourceKind,
    /// The adapter that listed the target.
    pub adapter: String,
    /// The target's id for that adapter.
    pub target: String,
    /// Its name; `null` lets the server pick one.
    #[serde(default)]
    pub name: Option<String>,
}

/// `source.delete`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRef {
    /// The source's id.
    pub source: u32,
}

/// `item.set_style` (protocol 1.3, ADR-0053): replaces the whole style of the item's
/// layout on the studio's canvas.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SetItemStyle {
    /// The item's id (`source.list` gives each video source's program-scene item).
    pub item: u32,
    /// The new style.
    pub style: ItemStyle,
}

/// `item.set_crop` (protocol 1.4, ADR-0057): the source pixels cut from each edge of
/// an item's picture, on the item's layout on the studio's canvas.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetItemCrop {
    /// The item's id (`source.list` gives each video source's program-scene item).
    pub item: u32,
    /// Pixels cut from the left, top, right and bottom.
    pub crop: [u32; 4],
    /// Keep the size of a source pixel on the canvas and the place of what stays (the
    /// box changes); `false` sets only the crop, and what stays fills the box. Default
    /// `true`.
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub keep_scale: bool,
}

fn yes() -> bool {
    true
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde passes a reference.
fn is_true(v: &bool) -> bool {
    *v
}

/// `record.start`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordStart {
    /// A recording profile's id (`output.profiles`). The file goes into its folder.
    pub profile: String,
}

/// `record.start_to_file` (`admin`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordStartToFile {
    /// A recording profile's id, for the encoding settings.
    pub profile: String,
    /// The file, absolute; refused with `file_exists` if it exists.
    pub path: String,
}

/// `settings.set`: the sections given replace the current ones.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingsPatch {
    /// New recording profiles.
    #[serde(default)]
    pub profiles: Option<Vec<RecordingProfile>>,
    /// A new canvas; the studio restarts with it. Refused while recording.
    #[serde(default)]
    pub canvas: Option<Canvas>,
    /// New stream settings (protocol 1.2). Refused while a stream runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<StreamSettingsPatch>,
}

/// `events.subscribe`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subscribe {
    /// Topics to add.
    pub topics: Vec<Topic>,
    /// Sampling rates in Hz for latest-value topics; capped per topic.
    #[serde(default)]
    pub rates: BTreeMap<Topic, u32>,
}

/// `events.unsubscribe`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unsubscribe {
    /// Topics to drop.
    pub topics: Vec<Topic>,
}

/// `tokens.create`: a named access token for apps (ADR-0044 decision 9).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateToken {
    /// Its name, unique; surrounding spaces are dropped.
    pub name: String,
    /// What it allows: `["read"]`, `["read", "control"]` or `["read", "control",
    /// "admin"]` (the access levels the desktop app offers).
    pub scopes: ScopeSet,
}

/// `tokens.revoke`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenRef {
    /// The token's name.
    pub name: String,
}

/// A request the control thread carries out.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// `studio.get`.
    StudioGet,
    /// `source.list`.
    SourceList,
    /// `source.targets`.
    SourceTargets(TargetsParams),
    /// `source.add`.
    SourceAdd(AddSource),
    /// `source.delete`.
    SourceDelete(SourceRef),
    /// `item.set_style` (protocol 1.3).
    ItemSetStyle(SetItemStyle),
    /// `item.set_crop` (protocol 1.4).
    ItemSetCrop(SetItemCrop),
    /// `edit.undo`.
    EditUndo,
    /// `edit.redo`.
    EditRedo,
    /// `output.profiles`.
    OutputProfiles,
    /// `record.start`.
    RecordStart(RecordStart),
    /// `record.start_to_file`.
    RecordStartToFile(RecordStartToFile),
    /// `record.stop`.
    RecordStop,
    /// `record.status`.
    RecordStatus,
    /// `stream.start`.
    StreamStart,
    /// `stream.stop`.
    StreamStop,
    /// `stream.status`.
    StreamStatus,
    /// `settings.get`.
    SettingsGet,
    /// `settings.set`.
    SettingsSet(SettingsPatch),
    /// `events.subscribe`: answered on the control thread with the `seq` of the topics'
    /// state keys, read after the subscriber mask was set. The target never sees it.
    EventsSubscribe(Subscribe),
    /// `tokens.list`.
    TokensList,
    /// `tokens.create`.
    TokensCreate(CreateToken),
    /// `tokens.revoke`.
    TokensRevoke(TokenRef),
}

impl Command {
    /// Its method name.
    pub fn method(&self) -> &'static str {
        match self {
            Command::StudioGet => "studio.get",
            Command::SourceList => "source.list",
            Command::SourceTargets(_) => "source.targets",
            Command::SourceAdd(_) => "source.add",
            Command::SourceDelete(_) => "source.delete",
            Command::ItemSetStyle(_) => "item.set_style",
            Command::ItemSetCrop(_) => "item.set_crop",
            Command::EditUndo => "edit.undo",
            Command::EditRedo => "edit.redo",
            Command::OutputProfiles => "output.profiles",
            Command::RecordStart(_) => "record.start",
            Command::RecordStartToFile(_) => "record.start_to_file",
            Command::RecordStop => "record.stop",
            Command::RecordStatus => "record.status",
            Command::StreamStart => "stream.start",
            Command::StreamStop => "stream.stop",
            Command::StreamStatus => "stream.status",
            Command::SettingsGet => "settings.get",
            Command::SettingsSet(_) => "settings.set",
            Command::EventsSubscribe(_) => "events.subscribe",
            Command::TokensList => "tokens.list",
            Command::TokensCreate(_) => "tokens.create",
            Command::TokensRevoke(_) => "tokens.revoke",
        }
    }
}

/// Everything a client may send.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(from = "WireRequest", into = "WireRequest")]
pub enum Request {
    /// `session.hello`.
    Hello(Hello),
    /// `session.authenticate`.
    Authenticate(Authenticate),
    /// `api.describe`.
    Describe,
    /// `events.unsubscribe`.
    Unsubscribe(Unsubscribe),
    /// A command for the control thread.
    Command(Command),
}

impl Request {
    /// Its method name.
    pub fn method(&self) -> &'static str {
        match self {
            Request::Hello(_) => "session.hello",
            Request::Authenticate(_) => "session.authenticate",
            Request::Describe => "api.describe",
            Request::Unsubscribe(_) => "events.unsubscribe",
            Request::Command(c) => c.method(),
        }
    }

    /// The scope it needs, or `None` for the handshake.
    pub fn scope(&self) -> Option<Scope> {
        method_scope(self.method()).flatten()
    }
}

/// A method and the scope it needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct MethodInfo {
    /// `namespace.verb`.
    pub name: &'static str,
    /// The scope it needs; `null` for the handshake.
    pub scope: Option<Scope>,
}

/// Every method of protocol version 1 and its scope (ADR-0044 decision 10).
pub const METHODS: &[MethodInfo] = &[
    m("session.hello", None),
    m("session.authenticate", None),
    m("api.describe", Some(Scope::Read)),
    m("studio.get", Some(Scope::Read)),
    m("source.list", Some(Scope::Read)),
    m("source.targets", Some(Scope::Control)),
    m("source.add", Some(Scope::Control)),
    m("source.delete", Some(Scope::Control)),
    m("item.set_style", Some(Scope::Control)),
    m("item.set_crop", Some(Scope::Control)),
    m("edit.undo", Some(Scope::Control)),
    m("edit.redo", Some(Scope::Control)),
    m("output.profiles", Some(Scope::Read)),
    m("record.start", Some(Scope::Control)),
    m("record.start_to_file", Some(Scope::Admin)),
    m("record.stop", Some(Scope::Control)),
    m("record.status", Some(Scope::Read)),
    m("stream.start", Some(Scope::Control)),
    m("stream.stop", Some(Scope::Control)),
    m("stream.status", Some(Scope::Read)),
    m("settings.get", Some(Scope::Admin)),
    m("settings.set", Some(Scope::Admin)),
    m("events.subscribe", Some(Scope::Read)),
    m("events.unsubscribe", Some(Scope::Read)),
    m("tokens.list", Some(Scope::Tokens)),
    m("tokens.create", Some(Scope::Tokens)),
    m("tokens.revoke", Some(Scope::Tokens)),
];

/// The client-scoped `ui.*` methods (control API spec, "Client-scoped UI commands").
/// The API thread routes them to a UI client, so they are not in [`METHODS`]; the
/// desktop UI's command registry may run them (ADR-0050, U20 test).
pub const UI_METHODS: &[&str] = &[
    "ui.workspace.list",
    "ui.workspace.switch",
    "ui.panel.open",
    "ui.panel.close",
    "ui.area.show",
    "ui.area.hide",
    "ui.area.toggle",
    "ui.panel.collapse",
    "ui.panel.expand",
    "ui.panel.auto_hide",
    "ui.panel.pin",
    "ui.area.hide_all",
    "ui.area.show_all",
    "ui.panel.maximize",
    "ui.panel.restore",
    "ui.layout.studio_split",
    "ui.layout.reset_sizes",
    "ui.toolbar.get",
    "ui.toolbar.set",
    "ui.toolbar.reset",
    "ui.command.list",
    "ui.command.run",
];

const fn m(name: &'static str, scope: Option<Scope>) -> MethodInfo {
    MethodInfo { name, scope }
}

/// The scope of a method name: `None` for an unknown method, `Some(None)` for the
/// handshake.
pub fn method_scope(name: &str) -> Option<Option<Scope>> {
    METHODS.iter().find(|i| i.name == name).map(|i| i.scope)
}

/// The scope needed to subscribe to a topic: `settings.changed` carries folders, which
/// only `admin` may read; every other topic needs `read`.
pub const fn topic_scope(topic: Topic) -> Scope {
    match topic {
        Topic::SettingsChanged => Scope::Admin,
        _ => Scope::Read,
    }
}

/// The flat wire form of [`Request`]: `{"method": ..., "params": ...}`.
#[derive(Serialize, Deserialize)]
#[serde(tag = "method", content = "params")]
enum WireRequest {
    #[serde(rename = "session.hello")]
    Hello(Hello),
    #[serde(rename = "session.authenticate")]
    Authenticate(Authenticate),
    #[serde(rename = "api.describe")]
    Describe(Empty),
    #[serde(rename = "studio.get")]
    StudioGet(Empty),
    #[serde(rename = "source.list")]
    SourceList(Empty),
    #[serde(rename = "source.targets")]
    SourceTargets(TargetsParams),
    #[serde(rename = "source.add")]
    SourceAdd(AddSource),
    #[serde(rename = "source.delete")]
    SourceDelete(SourceRef),
    #[serde(rename = "item.set_style")]
    ItemSetStyle(SetItemStyle),
    #[serde(rename = "item.set_crop")]
    ItemSetCrop(SetItemCrop),
    #[serde(rename = "edit.undo")]
    EditUndo(Empty),
    #[serde(rename = "edit.redo")]
    EditRedo(Empty),
    #[serde(rename = "output.profiles")]
    OutputProfiles(Empty),
    #[serde(rename = "record.start")]
    RecordStart(RecordStart),
    #[serde(rename = "record.start_to_file")]
    RecordStartToFile(RecordStartToFile),
    #[serde(rename = "record.stop")]
    RecordStop(Empty),
    #[serde(rename = "record.status")]
    RecordStatus(Empty),
    #[serde(rename = "stream.start")]
    StreamStart(Empty),
    #[serde(rename = "stream.stop")]
    StreamStop(Empty),
    #[serde(rename = "stream.status")]
    StreamStatus(Empty),
    #[serde(rename = "settings.get")]
    SettingsGet(Empty),
    #[serde(rename = "settings.set")]
    SettingsSet(SettingsPatch),
    #[serde(rename = "events.subscribe")]
    EventsSubscribe(Subscribe),
    #[serde(rename = "events.unsubscribe")]
    EventsUnsubscribe(Unsubscribe),
    #[serde(rename = "tokens.list")]
    TokensList(Empty),
    #[serde(rename = "tokens.create")]
    TokensCreate(CreateToken),
    #[serde(rename = "tokens.revoke")]
    TokensRevoke(TokenRef),
}

impl From<WireRequest> for Request {
    fn from(w: WireRequest) -> Self {
        use Command as C;
        match w {
            WireRequest::Hello(p) => Request::Hello(p),
            WireRequest::Authenticate(p) => Request::Authenticate(p),
            WireRequest::Describe(_) => Request::Describe,
            WireRequest::EventsUnsubscribe(p) => Request::Unsubscribe(p),
            WireRequest::StudioGet(_) => Request::Command(C::StudioGet),
            WireRequest::SourceList(_) => Request::Command(C::SourceList),
            WireRequest::SourceTargets(p) => Request::Command(C::SourceTargets(p)),
            WireRequest::SourceAdd(p) => Request::Command(C::SourceAdd(p)),
            WireRequest::SourceDelete(p) => Request::Command(C::SourceDelete(p)),
            WireRequest::ItemSetStyle(p) => Request::Command(C::ItemSetStyle(p)),
            WireRequest::ItemSetCrop(p) => Request::Command(C::ItemSetCrop(p)),
            WireRequest::EditUndo(_) => Request::Command(C::EditUndo),
            WireRequest::EditRedo(_) => Request::Command(C::EditRedo),
            WireRequest::OutputProfiles(_) => Request::Command(C::OutputProfiles),
            WireRequest::RecordStart(p) => Request::Command(C::RecordStart(p)),
            WireRequest::RecordStartToFile(p) => Request::Command(C::RecordStartToFile(p)),
            WireRequest::RecordStop(_) => Request::Command(C::RecordStop),
            WireRequest::RecordStatus(_) => Request::Command(C::RecordStatus),
            WireRequest::StreamStart(_) => Request::Command(C::StreamStart),
            WireRequest::StreamStop(_) => Request::Command(C::StreamStop),
            WireRequest::StreamStatus(_) => Request::Command(C::StreamStatus),
            WireRequest::SettingsGet(_) => Request::Command(C::SettingsGet),
            WireRequest::SettingsSet(p) => Request::Command(C::SettingsSet(p)),
            WireRequest::EventsSubscribe(p) => Request::Command(C::EventsSubscribe(p)),
            WireRequest::TokensList(_) => Request::Command(C::TokensList),
            WireRequest::TokensCreate(p) => Request::Command(C::TokensCreate(p)),
            WireRequest::TokensRevoke(p) => Request::Command(C::TokensRevoke(p)),
        }
    }
}

impl From<Request> for WireRequest {
    fn from(r: Request) -> Self {
        use Command as C;
        match r {
            Request::Hello(p) => WireRequest::Hello(p),
            Request::Authenticate(p) => WireRequest::Authenticate(p),
            Request::Describe => WireRequest::Describe(Empty {}),
            Request::Unsubscribe(p) => WireRequest::EventsUnsubscribe(p),
            Request::Command(c) => match c {
                C::StudioGet => WireRequest::StudioGet(Empty {}),
                C::SourceList => WireRequest::SourceList(Empty {}),
                C::SourceTargets(p) => WireRequest::SourceTargets(p),
                C::SourceAdd(p) => WireRequest::SourceAdd(p),
                C::SourceDelete(p) => WireRequest::SourceDelete(p),
                C::ItemSetStyle(p) => WireRequest::ItemSetStyle(p),
                C::ItemSetCrop(p) => WireRequest::ItemSetCrop(p),
                C::EditUndo => WireRequest::EditUndo(Empty {}),
                C::EditRedo => WireRequest::EditRedo(Empty {}),
                C::OutputProfiles => WireRequest::OutputProfiles(Empty {}),
                C::RecordStart(p) => WireRequest::RecordStart(p),
                C::RecordStartToFile(p) => WireRequest::RecordStartToFile(p),
                C::RecordStop => WireRequest::RecordStop(Empty {}),
                C::RecordStatus => WireRequest::RecordStatus(Empty {}),
                C::StreamStart => WireRequest::StreamStart(Empty {}),
                C::StreamStop => WireRequest::StreamStop(Empty {}),
                C::StreamStatus => WireRequest::StreamStatus(Empty {}),
                C::SettingsGet => WireRequest::SettingsGet(Empty {}),
                C::SettingsSet(p) => WireRequest::SettingsSet(p),
                C::EventsSubscribe(p) => WireRequest::EventsSubscribe(p),
                C::TokensList => WireRequest::TokensList(Empty {}),
                C::TokensCreate(p) => WireRequest::TokensCreate(p),
                C::TokensRevoke(p) => WireRequest::TokensRevoke(p),
            },
        }
    }
}

/// `session.hello`'s answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloResult {
    /// The protocol version the session speaks.
    pub protocol: String,
    /// A random id of this server run; `seq` counters restart with it.
    pub instance: String,
    /// On IPC, a verified same-user peer is authenticated by `session.hello` and gets
    /// its scopes here; otherwise `null`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scopes: Option<ScopeSet>,
}

/// `session.authenticate`'s answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthResult {
    /// What the session may do (the token's scopes, limited by the channel).
    pub scopes: ScopeSet,
    /// The server.
    pub server: ClientInfo,
}

/// `api.describe`'s answer. The JSON Schema joins it with the schema generator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Description {
    /// The protocol version the session speaks.
    pub protocol: String,
    /// Every method and its scope.
    pub methods: Vec<MethodInfoOwned>,
    /// Every event topic.
    pub topics: Vec<Topic>,
}

/// A method and its scope, owned (for deserializing).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MethodInfoOwned {
    /// `namespace.verb`.
    pub name: String,
    /// The scope it needs; `null` for the handshake.
    pub scope: Option<Scope>,
}

/// `source.targets`' answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetList {
    /// The kind.
    pub kind: SourceKind,
    /// Its capture targets, in OS order.
    pub targets: Vec<CaptureTarget>,
}

/// `source.add`'s answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceAdded {
    /// The new source's id.
    pub source: u32,
}

/// `output.profiles`' answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileList {
    /// The recording profiles.
    pub profiles: Vec<OutputProfile>,
}

/// `record.start`'s answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordStarted {
    /// The video encoder's name.
    pub video_encoder: String,
    /// The audio encoder's name, when audio is recorded.
    pub audio_encoder: Option<String>,
}

/// `stream.start`'s answer: the stream runs and connects in the background
/// (`stream.state` says when it reaches the server).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamStarted {
    /// The video encoder's name.
    pub video_encoder: String,
    /// The audio encoder's name, when audio is sent.
    pub audio_encoder: Option<String>,
}

/// `events.subscribe`'s answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subscribed {
    /// The current `seq` per state key of the subscribed state topics.
    pub seq: SeqMap,
}

/// An access token as `tokens.list` shows it: never the token or its hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenInfo {
    /// Its name.
    pub name: String,
    /// What it allows.
    pub scopes: ScopeSet,
}

/// `tokens.list`' answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenList {
    /// The tokens, oldest first.
    pub tokens: Vec<TokenInfo>,
}

/// `tokens.create`'s answer: the only time the token exists in readable form.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenCreated {
    /// Its name as kept (trimmed).
    pub name: String,
    /// What it allows.
    pub scopes: ScopeSet,
    /// The token's text (`rbs1_` and 64 hex digits).
    pub token: String,
}

impl std::fmt::Debug for TokenCreated {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenCreated")
            .field("name", &self.name)
            .field("scopes", &self.scopes)
            .field("token", &"<redacted>")
            .finish()
    }
}

/// A successful answer. Serialized as the JSON-RPC `result`; a client knows its type
/// from the method it called, so each result type deserializes on its own.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Response {
    /// `session.hello`.
    Hello(HelloResult),
    /// `session.authenticate`.
    Authenticated(AuthResult),
    /// `api.describe`.
    Describe(Description),
    /// `studio.get`.
    Studio(Snapshot<StudioInfo>),
    /// `source.list`.
    Sources(Snapshot<Vec<SourceInfo>>),
    /// `source.targets`.
    Targets(TargetList),
    /// `source.add`.
    SourceAdded(SourceAdded),
    /// `output.profiles`.
    Profiles(ProfileList),
    /// `record.start`, `record.start_to_file`.
    RecordStarted(RecordStarted),
    /// `record.status`.
    RecordStatus(Snapshot<RecordState>),
    /// `stream.start`.
    StreamStarted(StreamStarted),
    /// `stream.status`.
    StreamStatus(Snapshot<StreamState>),
    /// `settings.get`, `settings.set`.
    Settings(Snapshot<Settings>),
    /// `events.subscribe`.
    Subscribed(Subscribed),
    /// `tokens.list`.
    Tokens(TokenList),
    /// `tokens.create`.
    TokenCreated(TokenCreated),
    /// Methods with nothing to return: `source.delete`, `edit.undo`, `edit.redo`,
    /// `record.stop`, `stream.stop`, `events.unsubscribe`, `tokens.revoke`.
    Done(Empty),
}
