//! Messages the studio sends a running runner after `configure`, on the same pipe
//! (ADR-0132 decisions 5 and 8, ADR-0149 decisions 3 and 8): each is a 4-byte
//! little-endian length, then a kind byte and its body. Closing the pipe still means
//! stop.

use crate::input::InputEvent;
use crate::{MAX_PAYLOAD, PageOptions};

/// Reload the main frame; body: one byte, 1 to bypass the cache.
pub const KIND_RELOAD: u8 = 2;
/// Run a script once; body: the script's UTF-8 bytes, empty for the saved script.
pub const KIND_RUN_SCRIPT: u8 = 3;
/// New page options (CSS, zoom, script, timers, retry); body: the options as in
/// `configure` (the profile directory is ignored).
pub const KIND_PAGE: u8 = 4;
/// One input event (`crate::input`).
pub const KIND_INPUT: u8 = 5;
/// Whether a shown scene draws the page (ADR-0149 decision 5); body: one byte, 1 shown.
pub const KIND_SHOWN: u8 = 6;

/// A message to a running runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Control {
    /// `browser.reload` (ADR-0132 decision 5).
    Reload {
        /// Load again from the network, not the cache.
        bypass_cache: bool,
    },
    /// `browser.run_script` (ADR-0149 decision 3): `None` runs the saved script.
    RunScript(Option<String>),
    /// Live page options (ADR-0149 decision 1): applied without a restart.
    Page(Box<PageOptions>),
    /// An input event for the page (ADR-0132 decision 6).
    Input(InputEvent),
    /// A background page is hidden (false) or shown again (true) (ADR-0149 decision
    /// 5): hidden, the runner paints once per second without telling the page.
    Shown(bool),
}

/// Encodes a control message with its length prefix.
pub fn encode(message: &Control) -> Vec<u8> {
    let mut body = Vec::new();
    match message {
        Control::Reload { bypass_cache } => {
            body.push(KIND_RELOAD);
            body.push(u8::from(*bypass_cache));
        }
        Control::RunScript(script) => {
            body.push(KIND_RUN_SCRIPT);
            body.extend_from_slice(script.as_deref().unwrap_or_default().as_bytes());
        }
        Control::Page(page) => {
            body.push(KIND_PAGE);
            page.encode(&mut body);
        }
        Control::Input(event) => {
            body.push(KIND_INPUT);
            event.encode(&mut body);
        }
        Control::Shown(shown) => {
            body.push(KIND_SHOWN);
            body.push(u8::from(*shown));
        }
    }
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&u32::try_from(body.len()).unwrap_or(u32::MAX).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

/// Decodes one message body (without the length prefix). Unknown kinds and
/// malformed bodies are `None`, and the runner ignores them.
pub fn decode(body: &[u8]) -> Option<Control> {
    if body.len() > MAX_PAYLOAD {
        return None;
    }
    match *body.first()? {
        KIND_RELOAD => Some(Control::Reload {
            bypass_cache: *body.get(1)? != 0,
        }),
        KIND_RUN_SCRIPT => {
            let text = std::str::from_utf8(body.get(1..)?).ok()?;
            Some(Control::RunScript(
                (!text.is_empty()).then(|| text.to_owned()),
            ))
        }
        KIND_PAGE => PageOptions::decode(body.get(1..)?).map(|p| Control::Page(Box::new(p))),
        KIND_INPUT => InputEvent::decode(body.get(1..)?).map(Control::Input),
        KIND_SHOWN => Some(Control::Shown(*body.get(1)? != 0)),
        _ => None,
    }
}

/// Reads the next message from `input`: `Ok(None)` for one this build does not know,
/// `Err` when the pipe closed or a length is out of bounds (the runner then stops).
pub fn read(input: &mut impl std::io::Read) -> std::io::Result<Option<Control>> {
    let mut len = [0u8; 4];
    input.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len == 0 || len > MAX_PAYLOAD {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "control message length out of bounds",
        ));
    }
    let mut body = vec![0u8; len];
    input.read_exact(&mut body)?;
    Ok(decode(&body))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn messages_round_trip_and_unknown_kinds_are_skipped() {
        for m in [
            Control::Reload { bypass_cache: true },
            Control::Reload {
                bypass_cache: false,
            },
            Control::RunScript(None),
            Control::RunScript(Some("document.title = 'é'".into())),
            Control::Page(Box::new(PageOptions {
                css: "body { margin: 0 }".into(),
                zoom_percent: 150,
                script: "x()".into(),
                script_on_load: true,
                script_every_s: 5,
                reload_every_s: 60,
                retry_on_error: false,
                reload_on_file_change: true,
                profile_dir: String::new(),
            })),
            Control::Input(InputEvent::Text("ğ".into())),
            Control::Shown(false),
            Control::Shown(true),
        ] {
            let bytes = encode(&m);
            assert_eq!(read(&mut bytes.as_slice()).unwrap(), Some(m));
        }
        let unknown = [2, 0, 0, 0, 99, 1];
        assert_eq!(read(&mut unknown.as_slice()).unwrap(), None);
        let huge = u32::try_from(MAX_PAYLOAD + 1).unwrap().to_le_bytes();
        assert!(read(&mut huge.as_slice()).is_err());
        assert!(read(&mut [].as_slice()).is_err(), "a closed pipe stops");
    }
}
