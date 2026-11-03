//! AMF0 command bodies of the publish sequence, and the replies the client reads.

use rbs_media_format::amf0::{self, Decoded, Value};

/// Transaction ids of the commands that get replies.
pub(crate) const TXN_CONNECT: f64 = 1.0;
pub(crate) const TXN_CREATE_STREAM: f64 = 4.0;

/// Shown to the server as the encoder's name, as other encoders do (`FMLE/3.0` is
/// what ingest servers recognise as a live encoder).
const FLASH_VER: &str = "FMLE/3.0 (compatible; RBS)";

fn start(out: &mut Vec<u8>, name: &str, txn: f64) {
    amf0::string(out, name);
    amf0::number(out, txn);
}

/// `connect(1, {app, type, flashVer, tcUrl, objectEncoding: 0})`. AMF0 object
/// encoding is requested so the server answers in AMF0, the only encoding decoded.
pub(crate) fn connect(out: &mut Vec<u8>, app: &str, tc_url: &str) {
    start(out, "connect", TXN_CONNECT);
    amf0::object(
        out,
        &[
            ("app", Value::String(app)),
            ("type", Value::String("nonprivate")),
            ("flashVer", Value::String(FLASH_VER)),
            ("tcUrl", Value::String(tc_url)),
            ("objectEncoding", Value::Number(0.0)),
        ],
    );
}

/// `releaseStream(2, null, key)`, `FCPublish(3, null, key)`: the calls ingest
/// servers expect before `createStream` (some free a stale publisher of the key).
pub(crate) fn release_stream(out: &mut Vec<u8>, key: &str) {
    start(out, "releaseStream", 2.0);
    amf0::null(out);
    amf0::string(out, key);
}

pub(crate) fn fc_publish(out: &mut Vec<u8>, key: &str) {
    start(out, "FCPublish", 3.0);
    amf0::null(out);
    amf0::string(out, key);
}

/// `createStream(4, null)`.
pub(crate) fn create_stream(out: &mut Vec<u8>) {
    start(out, "createStream", TXN_CREATE_STREAM);
    amf0::null(out);
}

/// `publish(5, null, key, "live")`, sent on the created stream.
pub(crate) fn publish(out: &mut Vec<u8>, key: &str) {
    start(out, "publish", 5.0);
    amf0::null(out);
    amf0::string(out, key);
    amf0::string(out, "live");
}

/// `FCUnpublish(6, null, key)`, at a clean end.
pub(crate) fn fc_unpublish(out: &mut Vec<u8>, key: &str) {
    start(out, "FCUnpublish", 6.0);
    amf0::null(out);
    amf0::string(out, key);
}

/// `deleteStream(7, null, stream id)`, at a clean end.
pub(crate) fn delete_stream(out: &mut Vec<u8>, stream_id: u32) {
    start(out, "deleteStream", 7.0);
    amf0::null(out);
    amf0::number(out, f64::from(stream_id));
}

/// `@setDataFrame("onMetaData", {...})`: the metadata data message of a publisher.
pub(crate) fn set_data_frame(out: &mut Vec<u8>, metadata_body: impl FnOnce(&mut Vec<u8>)) {
    amf0::string(out, "@setDataFrame");
    metadata_body(out);
}

/// A command the server sent, decoded.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Reply {
    /// `_result` of transaction `txn`; `value` is the fourth value (the stream id
    /// for `createStream`, the information object for `connect`).
    Result { txn: f64, value: Option<Decoded> },
    /// `_error` of transaction `txn`.
    Error { txn: f64, info: Status },
    /// `onStatus` (also the `onFCPublish` family).
    Status(Status),
    /// Anything else (`onBWDone`, `onFCPublish`, ...): ignored.
    Other,
}

/// The `level`, `code` and `description` of a status object.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Status {
    pub level: String,
    pub code: String,
    pub description: String,
}

impl Status {
    fn from(value: Option<&Decoded>) -> Self {
        let field = |name| {
            value
                .and_then(|v| v.get(name))
                .and_then(Decoded::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        Self {
            level: field("level"),
            code: field("code"),
            description: field("description"),
        }
    }

    pub(crate) fn is_error(&self) -> bool {
        self.level.eq_ignore_ascii_case("error")
    }
}

/// Decodes a command message body (the caller has bounded its size).
pub(crate) fn parse_reply(body: &[u8]) -> Result<Reply, &'static str> {
    let values = amf0::decode_all(body).map_err(|_| "malformed AMF0 command")?;
    let name = values.first().and_then(Decoded::as_str).unwrap_or_default();
    let txn = values.get(1).and_then(Decoded::as_number).unwrap_or(0.0);
    Ok(match name {
        "_result" => Reply::Result {
            txn,
            value: values.get(3).cloned(),
        },
        "_error" => Reply::Error {
            txn,
            info: Status::from(values.get(3)),
        },
        "onStatus" => Reply::Status(Status::from(values.get(3))),
        _ => Reply::Other,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    #[test]
    fn connect_asks_for_amf0() {
        let mut out = Vec::new();
        connect(&mut out, "live", "rtmp://h/live");
        let values = amf0::decode_all(&out).unwrap();
        assert_eq!(values[0].as_str(), Some("connect"));
        assert_eq!(values[1].as_number(), Some(1.0));
        let obj = &values[2];
        assert_eq!(obj.get("app").and_then(Decoded::as_str), Some("live"));
        assert_eq!(
            obj.get("tcUrl").and_then(Decoded::as_str),
            Some("rtmp://h/live")
        );
        assert_eq!(
            obj.get("objectEncoding").and_then(Decoded::as_number),
            Some(0.0)
        );
    }

    #[test]
    fn parses_replies() {
        let mut body = Vec::new();
        amf0::string(&mut body, "_result");
        amf0::number(&mut body, 4.0);
        amf0::null(&mut body);
        amf0::number(&mut body, 1.0);
        assert_eq!(
            parse_reply(&body).unwrap(),
            Reply::Result {
                txn: 4.0,
                value: Some(Decoded::Number(1.0))
            }
        );
        let mut body = Vec::new();
        amf0::string(&mut body, "onStatus");
        amf0::number(&mut body, 0.0);
        amf0::null(&mut body);
        amf0::object(
            &mut body,
            &[
                ("level", Value::String("error")),
                ("code", Value::String("NetStream.Publish.BadName")),
            ],
        );
        let Reply::Status(status) = parse_reply(&body).unwrap() else {
            panic!("not a status");
        };
        assert!(status.is_error());
        assert_eq!(status.code, "NetStream.Publish.BadName");
        assert!(
            parse_reply(&[0x07, 0, 1]).is_err(),
            "references are refused"
        );
    }
}
