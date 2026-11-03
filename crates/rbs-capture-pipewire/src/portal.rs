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

/// Longest wait for a portal call that shows no dialog.
const CALL_TIMEOUT: Duration = Duration::from_secs(2);

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
    /// The portal did not answer in time.
    pub const TIMEOUT: i64 = 3;
    /// The portal returned no stream.
    pub const NO_STREAM: i64 = 4;
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
    /// Longest wait for the `Start` answer; `None` waits for the user.
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

impl Cast {
    /// Creates a portal session, selects the sources and starts it. With
    /// `choice.restore_token` and a valid token the portal shows no dialog.
    pub(crate) fn start(choice: &CastRequest<'_>) -> Result<Cast, CaptureError> {
        let types = source_type(choice.kind).ok_or(CaptureError::Unsupported(
            "the portal casts displays and windows",
        ))?;
        let connection = Connection::new_session().map_err(|_| dbus_error(code::CONNECT))?;
        let portal = Portal {
            connection: &connection,
        };
        let available = portal.property_u32("AvailableSourceTypes").unwrap_or(0);
        if available & types == 0 {
            return Err(CaptureError::Unsupported(
                "the desktop portal cannot cast this kind of target",
            ));
        }
        let cursors = portal.property_u32("AvailableCursorModes").unwrap_or(0);
        let deadline = Instant::now() + CALL_TIMEOUT;

        let mut options = PropMap::new();
        options.insert("session_handle_token".into(), variant(next_token()));
        let results = portal.request("CreateSession", (), options, Some(deadline))?;
        let session = results
            .get("session_handle")
            .and_then(|v| v.0.as_str())
            .and_then(|s| Path::new(s.to_owned()).ok())
            .ok_or(dbus_error(code::CALL))?;
        let mut cast = Cast {
            connection,
            session,
            stream: Stream {
                node: 0,
                size: None,
                restore_token: None,
            },
        };
        let portal = Portal {
            connection: &cast.connection,
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
        let selected = portal.request(
            "SelectSources",
            (cast.session.clone(),),
            options,
            Some(deadline),
        );
        if let Err(e) = selected {
            cast.close();
            return Err(e);
        }

        let started = portal.request(
            "Start",
            (cast.session.clone(), ""),
            PropMap::new(),
            choice.timeout.map(|t| Instant::now() + t),
        );
        let results = match started {
            Ok(results) => results,
            Err(e) => {
                cast.close();
                return Err(e);
            }
        };
        match parse_stream(&results) {
            Some(stream) => cast.stream = stream,
            None => {
                cast.close();
                return Err(dbus_error(code::NO_STREAM));
            }
        }
        Ok(cast)
    }

    /// A connection to the PipeWire daemon that only sees this cast's stream.
    pub(crate) fn open_remote(&self) -> Result<OwnedFd, CaptureError> {
        let proxy = self
            .connection
            .with_proxy(DESTINATION, OBJECT, CALL_TIMEOUT);
        let (fd,): (OwnedFd,) = proxy
            .method_call(
                SCREEN_CAST,
                "OpenPipeWireRemote",
                (self.session.clone(), PropMap::new()),
            )
            .map_err(|_| dbus_error(code::CALL))?;
        Ok(fd)
    }

    /// Ends the cast. Dropping the connection afterwards would end it too, but closing
    /// explicitly lets the compositor stop at once.
    pub(crate) fn close(&mut self) {
        let proxy = self
            .connection
            .with_proxy(DESTINATION, self.session.clone(), CALL_TIMEOUT);
        let _: Result<(), _> = proxy.method_call(SESSION, "Close", ());
    }
}

/// One portal call on a borrowed connection.
struct Portal<'a> {
    connection: &'a Connection,
}

impl Portal<'_> {
    fn property_u32(&self, name: &str) -> Option<u32> {
        use dbus::blocking::stdintf::org_freedesktop_dbus::Properties;
        let proxy = self
            .connection
            .with_proxy(DESTINATION, OBJECT, CALL_TIMEOUT);
        proxy.get::<u32>(SCREEN_CAST, name).ok()
    }

    /// Calls `method` with `args` followed by `options` (which gets a `handle_token`)
    /// and waits for the request's `Response` until `deadline` (`None`: no limit).
    fn request<A: dbus::arg::AppendAll>(
        &self,
        method: &str,
        args: A,
        mut options: PropMap,
        deadline: Option<Instant>,
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
        let rule = response_rule(&path);
        self.connection
            .add_match_no_cb(&rule)
            .map_err(|_| dbus_error(code::CALL))?;
        options.insert("handle_token".into(), variant(token));

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
            .send_with_reply_and_block(message, CALL_TIMEOUT);
        let result = match reply {
            Ok(reply) => {
                // Portals older than 0.9 ignore `handle_token` and pick their own path.
                let handle = reply.read1::<Path<'static>>().ok();
                let path = handle.unwrap_or(path);
                let other = response_rule(&path);
                if other != rule {
                    let _ = self.connection.add_match_no_cb(&other);
                }
                let answer = self.wait_response(&path, deadline);
                if answer.is_err() {
                    // Dismisses a dialog the portal may still show.
                    let proxy = self
                        .connection
                        .with_proxy(DESTINATION, path.clone(), CALL_TIMEOUT);
                    let _: Result<(), _> = proxy.method_call(REQUEST, "Close", ());
                }
                if other != rule {
                    let _ = self.connection.remove_match_no_cb(&other);
                }
                answer
            }
            Err(_) => Err(dbus_error(code::CALL)),
        };
        let _ = self.connection.remove_match_no_cb(&rule);
        result
    }

    fn wait_response(
        &self,
        path: &Path<'_>,
        deadline: Option<Instant>,
    ) -> Result<PropMap, CaptureError> {
        let interface = Interface::new(REQUEST).map_err(|_| dbus_error(code::CALL))?;
        let member = Member::new("Response").map_err(|_| dbus_error(code::CALL))?;
        loop {
            let wait = match deadline {
                Some(deadline) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err(dbus_error(code::TIMEOUT));
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
            if message.msg_type() != MessageType::Signal
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

fn response_rule(path: &Path<'_>) -> String {
    MatchRule::new_signal(REQUEST, "Response")
        .with_path(path.clone())
        .match_str()
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
