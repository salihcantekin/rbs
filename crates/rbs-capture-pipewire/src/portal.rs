//! The XDG desktop portal's ScreenCast interface (`org.freedesktop.portal.ScreenCast`)
//! over the session bus.
//!
//! Every portal request answers through a `Response` signal on a request object, so
//! each call here subscribes to that signal, calls the method and reads messages from
//! its own private connection until the answer arrives. The connection is plain
//! libdbus: it starts no threads, so a closed [`Cast`] leaves nothing behind (P13).
//! Only control threads use this module.

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use dbus::arg::{PropMap, RefArg, Variant};
use dbus::blocking::Connection;
use dbus::message::{MatchRule, MessageType};
use dbus::strings::{Interface, Member};
use dbus::{Message, Path};

use rbs_capture::{CaptureError, ErrorDomain, TargetKind};

const DESTINATION: &str = "org.freedesktop.portal.Desktop";
const OBJECT: &str = "/org/freedesktop/portal/desktop";
const SCREEN_CAST: &str = "org.freedesktop.portal.ScreenCast";
const REQUEST: &str = "org.freedesktop.portal.Request";
const SESSION: &str = "org.freedesktop.portal.Session";

/// Budget for every portal call of one `Cast::start` or `pick` that shows no dialog,
/// together: property reads, `CreateSession`, `SelectSources`, `OpenPipeWireRemote`.
const SETUP_BUDGET: Duration = Duration::from_secs(2);

/// Longest wait for closing a request or session.
const CLOSE_TIMEOUT: Duration = Duration::from_millis(500);

/// Source types (`AvailableSourceTypes`, `types`).
const SOURCE_MONITOR: u32 = 1;
const SOURCE_WINDOW: u32 = 2;

/// Cursor modes (`AvailableCursorModes`, `cursor_mode`).
const CURSOR_HIDDEN: u32 = 1;
const CURSOR_EMBEDDED: u32 = 2;

/// `persist_mode`: keep the permission until the user revokes it.
const PERSIST_UNTIL_REVOKED: u32 = 2;

/// Portal response codes.
const RESPONSE_SUCCESS: u32 = 0;
const RESPONSE_CANCELLED: u32 = 1;

/// Makes handle tokens unique within the process.
static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

/// A backend error in the D-Bus domain. `code` names the failing step.
pub(crate) fn dbus_error(code: i64) -> CaptureError {
    CaptureError::Backend {
        domain: ErrorDomain::Library,
        code,
        recoverable: true,
    }
}

/// Error codes of [`dbus_error`]: which step failed.
pub(crate) mod code {
    /// No session bus.
    pub const CONNECT: i64 = 1;
    /// A portal call failed or answered something unexpected.
    pub const CALL: i64 = 2;
    /// A portal call that shows no dialog did not answer in time.
    pub const TIMEOUT: i64 = 3;
    /// The portal returned no stream.
    pub const NO_STREAM: i64 = 4;
    /// The portal left the bus while a request was pending.
    pub const PORTAL_GONE: i64 = 5;
    /// `Start` did not answer within the caller's limit: the portal shows its dialog.
    pub const DIALOG_TIMEOUT: i64 = 6;
}

/// Whether a session bus address is known for this process. Does not connect.
pub(crate) fn bus_present() -> bool {
    if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some_and(|a| !a.is_empty()) {
        return true;
    }
    std::env::var_os("XDG_RUNTIME_DIR")
        .is_some_and(|dir| std::path::Path::new(&dir).join("bus").exists())
}

/// What the user chose, and how to open it again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Stream {
    /// The PipeWire node id, valid on the remote from [`Cast::open_remote`].
    pub node: u32,
    /// Logical size reported by the portal, if any.
    pub size: Option<(u32, u32)>,
    /// A single-use token that restores the same choice without a dialog.
    pub restore_token: Option<String>,
}

/// A started portal screen-cast session. Closing it (or dropping it, which closes the
/// connection) ends the cast; the compositor then removes the stream's node.
pub(crate) struct Cast {
    connection: Connection,
    session: Path<'static>,
    /// What is left of [`SETUP_BUDGET`] for `open_remote`.
    budget: Duration,
    pub stream: Stream,
}

impl std::fmt::Debug for Cast {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cast")
            .field("session", &self.session)
            .field("stream", &self.stream)
            .finish_non_exhaustive()
    }
}

/// How a cast is started.
pub(crate) struct CastRequest<'a> {
    pub kind: TargetKind,
    pub cursor: bool,
    /// Restore a previous choice; the portal shows its dialog if the token is no longer
    /// valid.
    pub restore_token: Option<&'a str>,
    /// Longest wait for the `Start` answer (`code::DIALOG_TIMEOUT` after it); `None`
    /// waits for the user.
    pub timeout: Option<Duration>,
}

fn variant<T: RefArg + 'static>(value: T) -> Variant<Box<dyn RefArg>> {
    Variant(Box::new(value))
}

fn next_token() -> String {
    format!("rbs{}", NEXT_TOKEN.fetch_add(1, Ordering::Relaxed))
}

fn source_type(kind: TargetKind) -> Option<u32> {
    match kind {
        TargetKind::Display => Some(SOURCE_MONITOR),
        TargetKind::Window => Some(SOURCE_WINDOW),
        _ => None,
    }
}

/// Time left until `deadline`, or the timeout error.
fn left(deadline: Instant) -> Result<Duration, CaptureError> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        Err(dbus_error(code::TIMEOUT))
    } else {
        Ok(left)
    }
}

/// A failed call: a timeout or another failure.
fn call_error(e: &dbus::Error) -> CaptureError {
    match e.name() {
        Some("org.freedesktop.DBus.Error.NoReply" | "org.freedesktop.DBus.Error.Timeout") => {
            dbus_error(code::TIMEOUT)
        }
        _ => dbus_error(code::CALL),
    }
}

impl Cast {
    /// Creates a portal session, selects the sources and starts it. With
    /// `choice.restore_token` and a valid token the portal shows no dialog.
    ///
    /// Every call that shows no dialog shares one [`SETUP_BUDGET`]; `Start` waits
    /// `choice.timeout` on top.
    pub(crate) fn start(choice: &CastRequest<'_>) -> Result<Cast, CaptureError> {
        let types = source_type(choice.kind).ok_or(CaptureError::Unsupported(
            "the portal casts displays and windows",
        ))?;
        let setup = Instant::now() + SETUP_BUDGET;
        let connection = Connection::new_session().map_err(|_| dbus_error(code::CONNECT))?;
        // Reading a property starts the portal if it is D-Bus activated.
        let available = property_u32(&connection, "AvailableSourceTypes", setup)?;
        if available & types == 0 {
            return Err(CaptureError::Unsupported(
                "the desktop portal cannot cast this kind of target",
            ));
        }
        let cursors = property_u32(&connection, "AvailableCursorModes", setup)?;
        let owner = name_owner(&connection, setup)?;
        // Notices the portal leaving the bus while a dialog is up.
        let gone = MatchRule::new_signal("org.freedesktop.DBus", "NameOwnerChanged")
            .with_sender("org.freedesktop.DBus")
            .match_str()
            + &format!(",arg0='{DESTINATION}'");
        connection
            .add_match_no_cb(&gone)
            .map_err(|_| dbus_error(code::CALL))?;
        let portal = Portal {
            connection: &connection,
            owner: &owner,
        };

        let mut options = PropMap::new();
        options.insert("session_handle_token".into(), variant(next_token()));
        let results = portal.request("CreateSession", (), options, setup, None)?;
        let session = results
            .get("session_handle")
            .and_then(|v| v.0.as_str())
            .and_then(|s| Path::new(s.to_owned()).ok())
            .ok_or(dbus_error(code::CALL))?;
        let mut cast = Cast {
            connection,
            session,
            budget: Duration::ZERO,
            stream: Stream {
                node: 0,
                size: None,
                restore_token: None,
            },
        };
        let result = cast.select_and_start(choice, types, cursors, &owner, setup);
        if let Err(e) = result {
            cast.close();
            return Err(e);
        }
        let _ = cast.connection.remove_match_no_cb(&gone);
        Ok(cast)
    }

    fn select_and_start(
        &mut self,
        choice: &CastRequest<'_>,
        types: u32,
        cursors: u32,
        owner: &str,
        setup: Instant,
    ) -> Result<(), CaptureError> {
        let portal = Portal {
            connection: &self.connection,
            owner,
        };
        let mut options = PropMap::new();
        options.insert("types".into(), variant(types));
        options.insert("multiple".into(), variant(false));
        let cursor = if choice.cursor && cursors & CURSOR_EMBEDDED != 0 {
            CURSOR_EMBEDDED
        } else {
            CURSOR_HIDDEN
        };
        if cursors & cursor != 0 {
            options.insert("cursor_mode".into(), variant(cursor));
        }
        options.insert("persist_mode".into(), variant(PERSIST_UNTIL_REVOKED));
        if let Some(token) = choice.restore_token {
            options.insert("restore_token".into(), variant(token.to_owned()));
        }
        portal.request(
            "SelectSources",
            (self.session.clone(),),
            options,
            setup,
            None,
        )?;

        // The dialog's time does not count against the setup budget.
        let budget = left(setup)?;
        let dialog = choice.timeout.map(|t| Instant::now() + t);
        let results = portal.request(
            "Start",
            (self.session.clone(), ""),
            PropMap::new(),
            setup,
            Some(dialog),
        )?;
        self.budget = budget;
        self.stream = parse_stream(&results).ok_or(dbus_error(code::NO_STREAM))?;
        Ok(())
    }

    /// A connection to the PipeWire daemon that only sees this cast's stream.
    pub(crate) fn open_remote(&self) -> Result<OwnedFd, CaptureError> {
        if self.budget.is_zero() {
            return Err(dbus_error(code::TIMEOUT));
        }
        let proxy = self.connection.with_proxy(DESTINATION, OBJECT, self.budget);
        let (fd,): (OwnedFd,) = proxy
            .method_call(
                SCREEN_CAST,
                "OpenPipeWireRemote",
                (self.session.clone(), PropMap::new()),
            )
            .map_err(|e| call_error(&e))?;
        Ok(fd)
    }

    /// Ends the cast. Dropping the connection afterwards would end it too, but closing
    /// explicitly lets the compositor stop at once.
    pub(crate) fn close(&mut self) {
        let proxy = self
            .connection
            .with_proxy(DESTINATION, self.session.clone(), CLOSE_TIMEOUT);
        let _: Result<(), _> = proxy.method_call(SESSION, "Close", ());
    }
}

fn property_u32(connection: &Connection, name: &str, setup: Instant) -> Result<u32, CaptureError> {
    use dbus::blocking::stdintf::org_freedesktop_dbus::Properties;
    let proxy = connection.with_proxy(DESTINATION, OBJECT, left(setup)?);
    proxy
        .get::<u32>(SCREEN_CAST, name)
        .map_err(|e| call_error(&e))
}

fn name_owner(connection: &Connection, setup: Instant) -> Result<String, CaptureError> {
    let proxy = connection.with_proxy(
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        left(setup)?,
    );
    let (owner,): (String,) = proxy
        .method_call("org.freedesktop.DBus", "GetNameOwner", (DESTINATION,))
        .map_err(|e| call_error(&e))?;
    Ok(owner)
}

/// Portal calls on a borrowed connection.
struct Portal<'a> {
    connection: &'a Connection,
    /// The unique name of the portal; responses from anyone else are ignored.
    owner: &'a str,
}

impl Portal<'_> {
    /// Calls `method` with `args` followed by `options` (which gets a `handle_token`)
    /// and waits for the request's `Response`: until `setup` (`code::TIMEOUT`) when
    /// `dialog` is `None`, otherwise until the dialog deadline (`Some(None)`: no limit;
    /// `code::DIALOG_TIMEOUT` after it).
    fn request<A: dbus::arg::AppendAll>(
        &self,
        method: &str,
        args: A,
        mut options: PropMap,
        setup: Instant,
        dialog: Option<Option<Instant>>,
    ) -> Result<PropMap, CaptureError> {
        let token = next_token();
        let sender = self
            .connection
            .channel()
            .unique_name()
            .ok_or(dbus_error(code::CONNECT))?
            .trim_start_matches(':')
            .replace('.', "_");
        let expected = format!("{OBJECT}/request/{sender}/{token}");
        let path = Path::new(expected).map_err(|_| dbus_error(code::CALL))?;
        // Subscribed before the call, so a fast answer is not missed.
        let rule = self.response_rule(&path);
        self.connection
            .add_match_no_cb(&rule)
            .map_err(|_| dbus_error(code::CALL))?;
        options.insert("handle_token".into(), variant(token));

        let result = (|| {
            let mut message = Message::new_method_call(DESTINATION, OBJECT, SCREEN_CAST, method)
                .map_err(|_| dbus_error(code::CALL))?;
            {
                let mut appender = dbus::arg::IterAppend::new(&mut message);
                args.append(&mut appender);
                appender.append(options);
            }
            let reply = self
                .connection
                .channel()
                .send_with_reply_and_block(message, left(setup)?)
                .map_err(|e| call_error(&e))?;
            // Every supported portal (1.0 and later) uses the path built from
            // `handle_token`; one that does not cannot be followed safely.
            let handle = reply.read1::<Path<'static>>().ok();
            if handle.as_ref().is_some_and(|h| *h != path) {
                return Err(dbus_error(code::CALL));
            }
            let (deadline, timeout_code) = match dialog {
                None => (Some(setup), code::TIMEOUT),
                Some(limit) => (limit, code::DIALOG_TIMEOUT),
            };
            let answer = self.wait_response(&path, deadline, timeout_code);
            if answer.is_err() {
                // Dismisses a dialog the portal may still show.
                let proxy = self
                    .connection
                    .with_proxy(DESTINATION, path.clone(), CLOSE_TIMEOUT);
                let _: Result<(), _> = proxy.method_call(REQUEST, "Close", ());
            }
            answer
        })();
        let _ = self.connection.remove_match_no_cb(&rule);
        result
    }

    fn response_rule(&self, path: &Path<'_>) -> String {
        MatchRule::new_signal(REQUEST, "Response")
            .with_path(path.clone())
            .with_sender(self.owner.to_owned())
            .match_str()
    }

    fn wait_response(
        &self,
        path: &Path<'_>,
        deadline: Option<Instant>,
        timeout_code: i64,
    ) -> Result<PropMap, CaptureError> {
        let interface = Interface::new(REQUEST).map_err(|_| dbus_error(code::CALL))?;
        let member = Member::new("Response").map_err(|_| dbus_error(code::CALL))?;
        loop {
            let wait = match deadline {
                Some(deadline) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err(dbus_error(timeout_code));
                    }
                    left
                }
                None => Duration::from_secs(3600),
            };
            let message = self
                .connection
                .channel()
                .blocking_pop_message(wait)
                .map_err(|_| dbus_error(code::CONNECT))?;
            let Some(message) = message else {
                continue;
            };
            if message.msg_type() != MessageType::Signal {
                continue;
            }
            if message.member().as_deref() == Some("NameOwnerChanged") {
                if let Ok((name, _, new)) = message.read3::<String, String, String>() {
                    if name == DESTINATION && new != self.owner {
                        return Err(dbus_error(code::PORTAL_GONE));
                    }
                }
                continue;
            }
            if message.sender().as_deref() != Some(self.owner)
                || message.path().as_ref() != Some(path)
                || message.interface().as_ref() != Some(&interface)
                || message.member().as_ref() != Some(&member)
            {
                continue;
            }
            let (response, results): (u32, PropMap) =
                message.read2().map_err(|_| dbus_error(code::CALL))?;
            return match response {
                RESPONSE_SUCCESS => Ok(results),
                RESPONSE_CANCELLED => Err(CaptureError::Cancelled),
                _ => Err(CaptureError::PermissionDenied),
            };
        }
    }
}

/// The first stream of a `Start` answer, and the restore token.
fn parse_stream(results: &PropMap) -> Option<Stream> {
    let streams = results.get("streams")?;
    let mut items = streams.0.as_iter()?;
    let first = items.next()?;
    let mut fields = first.as_iter()?;
    let node = u32::try_from(fields.next()?.as_u64()?).ok()?;
    let props = dict(fields.next()?);
    let size = props.get("size").and_then(|v| {
        let mut pair = v.as_iter()?;
        let w = u32::try_from(pair.next()?.as_i64()?).ok()?;
        let h = u32::try_from(pair.next()?.as_i64()?).ok()?;
        Some((w, h))
    });
    let restore_token = results
        .get("restore_token")
        .and_then(|v| v.0.as_str())
        .filter(|t| !t.is_empty())
        .map(str::to_owned);
    Some(Stream {
        node,
        size,
        restore_token,
    })
}

/// The entries of an `a{sv}` held as a `RefArg`, values unwrapped from their variants.
fn dict(value: &dyn RefArg) -> HashMap<String, &dyn RefArg> {
    let mut map = HashMap::new();
    let Some(mut items) = value.as_iter() else {
        return map;
    };
    while let (Some(key), Some(value)) = (items.next(), items.next()) {
        if let Some(key) = key.as_str() {
            // A variant iterates over its single inner value.
            let inner = value.as_iter().and_then(|mut v| v.next()).unwrap_or(value);
            map.insert(key.to_owned(), inner);
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    fn streams_answer(token: Option<&str>) -> PropMap {
        let mut props = PropMap::new();
        props.insert("size".into(), variant((1920i32, 1080i32)));
        props.insert("source_type".into(), variant(SOURCE_MONITOR));
        let streams: Vec<(u32, PropMap)> = vec![(42, props)];
        let mut results = PropMap::new();
        results.insert("streams".into(), variant(streams));
        if let Some(token) = token {
            results.insert("restore_token".into(), variant(token.to_owned()));
        }
        results
    }

    #[test]
    fn parses_the_start_answer() {
        let stream = parse_stream(&streams_answer(Some("abc")));
        assert_eq!(
            stream,
            Some(Stream {
                node: 42,
                size: Some((1920, 1080)),
                restore_token: Some("abc".into()),
            })
        );
        let stream = parse_stream(&streams_answer(None));
        assert_eq!(stream.and_then(|s| s.restore_token), None);
    }

    #[test]
    fn an_answer_without_streams_is_refused() {
        assert_eq!(parse_stream(&PropMap::new()), None);
        let mut results = PropMap::new();
        results.insert("streams".into(), variant(Vec::<(u32, PropMap)>::new()));
        assert_eq!(parse_stream(&results), None);
    }

    #[test]
    fn source_types_follow_the_kind() {
        assert_eq!(source_type(TargetKind::Display), Some(SOURCE_MONITOR));
        assert_eq!(source_type(TargetKind::Window), Some(SOURCE_WINDOW));
        assert_eq!(source_type(TargetKind::Camera), None);
    }
}
