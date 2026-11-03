//! AMF0 (Action Message Format 0) values, as FLV script tags and RTMP commands use.
//!
//! Writers append to a caller-owned `Vec<u8>`. [`decode`] reads one value into an
//! owned [`Decoded`] tree; it allocates and is meant for control messages (RTMP
//! command replies), not for per-packet work. Nesting is bounded by [`MAX_DEPTH`].

use crate::FormatError;

const NUMBER: u8 = 0x00;
const BOOLEAN: u8 = 0x01;
const STRING: u8 = 0x02;
const OBJECT: u8 = 0x03;
const NULL: u8 = 0x05;
const UNDEFINED: u8 = 0x06;
const ECMA_ARRAY: u8 = 0x08;
const OBJECT_END: u8 = 0x09;
const STRICT_ARRAY: u8 = 0x0a;
const DATE: u8 = 0x0b;
const LONG_STRING: u8 = 0x0c;

/// Deepest nesting of objects and arrays that [`decode`] accepts.
pub const MAX_DEPTH: usize = 16;

/// A value to write.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Value<'a> {
    /// AMF0 number (IEEE 754 double).
    Number(f64),
    /// AMF0 boolean.
    Bool(bool),
    /// AMF0 string (a long string past 65 535 bytes).
    String(&'a str),
    /// AMF0 null.
    Null,
}

/// Appends a value.
pub fn value(out: &mut Vec<u8>, value: &Value<'_>) {
    match value {
        Value::Number(n) => number(out, *n),
        Value::Bool(b) => boolean(out, *b),
        Value::String(s) => string(out, s),
        Value::Null => null(out),
    }
}

/// Appends a string value: marker 0x02 with a 16-bit length, or a long string
/// (marker 0x0c, 32-bit length) when it does not fit.
pub fn string(out: &mut Vec<u8>, value: &str) {
    match u16::try_from(value.len()) {
        Ok(len) => {
            out.push(STRING);
            out.extend_from_slice(&len.to_be_bytes());
        }
        Err(_) => {
            out.push(LONG_STRING);
            let len = u32::try_from(value.len()).unwrap_or(u32::MAX);
            out.extend_from_slice(&len.to_be_bytes());
            out.extend_from_slice(&value.as_bytes()[..len as usize]);
            return;
        }
    }
    out.extend_from_slice(value.as_bytes());
}

/// Appends an object or ECMA-array key: a 16-bit length and UTF-8 bytes. Keys longer
/// than 65 535 bytes are cut at a character boundary.
pub fn key(out: &mut Vec<u8>, value: &str) {
    let mut end = value.len().min(usize::from(u16::MAX));
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    out.extend_from_slice(&(end as u16).to_be_bytes());
    out.extend_from_slice(&value.as_bytes()[..end]);
}

/// Appends a number (marker 0x00).
pub fn number(out: &mut Vec<u8>, value: f64) {
    out.push(NUMBER);
    out.extend_from_slice(&value.to_be_bytes());
}

/// Appends a boolean (marker 0x01).
pub fn boolean(out: &mut Vec<u8>, value: bool) {
    out.extend_from_slice(&[BOOLEAN, u8::from(value)]);
}

/// Appends null (marker 0x05).
pub fn null(out: &mut Vec<u8>) {
    out.push(NULL);
}

/// Appends an anonymous object (marker 0x03) of `(key, value)` entries.
pub fn object(out: &mut Vec<u8>, entries: &[(&str, Value<'_>)]) {
    out.push(OBJECT);
    properties(out, entries);
}

/// Appends an ECMA array (marker 0x08) of `(key, value)` entries.
pub fn ecma_array(out: &mut Vec<u8>, entries: &[(&str, Value<'_>)]) {
    out.push(ECMA_ARRAY);
    out.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    properties(out, entries);
}

fn properties(out: &mut Vec<u8>, entries: &[(&str, Value<'_>)]) {
    for (name, v) in entries {
        key(out, name);
        value(out, v);
    }
    // Object end: empty key, then marker 0x09.
    out.extend_from_slice(&[0, 0, OBJECT_END]);
}

/// A value read by [`decode`].
#[derive(Debug, Clone, PartialEq)]
pub enum Decoded {
    /// Number.
    Number(f64),
    /// Boolean.
    Bool(bool),
    /// String or long string (invalid UTF-8 is replaced).
    String(String),
    /// Object or ECMA array, in wire order.
    Object(Vec<(String, Decoded)>),
    /// Strict array.
    Array(Vec<Decoded>),
    /// Date: milliseconds since the Unix epoch (the time zone field is ignored).
    Date(f64),
    /// Null.
    Null,
    /// Undefined.
    Undefined,
}

impl Decoded {
    /// The number, if this is one.
    pub fn as_number(&self) -> Option<f64> {
        match self {
            Decoded::Number(n) => Some(*n),
            _ => None,
        }
    }

    /// The string, if this is one.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Decoded::String(s) => Some(s),
            _ => None,
        }
    }

    /// The value of the first property named `name`, if this is an object.
    pub fn get(&self, name: &str) -> Option<&Decoded> {
        match self {
            Decoded::Object(entries) => entries.iter().find(|(k, _)| k == name).map(|(_, v)| v),
            _ => None,
        }
    }
}

/// Reads one value from the start of `input`; returns it and the bytes it used.
pub fn decode(input: &[u8]) -> Result<(Decoded, usize), FormatError> {
    let mut reader = Reader {
        data: input,
        pos: 0,
    };
    let value = reader.value(0)?;
    Ok((value, reader.pos))
}

/// Reads values until `input` ends (an RTMP command message body).
pub fn decode_all(mut input: &[u8]) -> Result<Vec<Decoded>, FormatError> {
    let mut values = Vec::new();
    while !input.is_empty() {
        let (value, used) = decode(input)?;
        values.push(value);
        input = &input[used..];
    }
    Ok(values)
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], FormatError> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|end| *end <= self.data.len())
            .ok_or(FormatError::Truncated("AMF0 value"))?;
        let bytes = &self.data[self.pos..end];
        self.pos = end;
        Ok(bytes)
    }

    fn u8(&mut self) -> Result<u8, FormatError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<usize, FormatError> {
        let b = self.take(2)?;
        Ok(usize::from(u16::from_be_bytes([b[0], b[1]])))
    }

    fn u32(&mut self) -> Result<usize, FormatError> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize)
    }

    fn f64(&mut self) -> Result<f64, FormatError> {
        let b = self.take(8)?;
        let mut raw = [0; 8];
        raw.copy_from_slice(b);
        Ok(f64::from_be_bytes(raw))
    }

    fn text(&mut self, len: usize) -> Result<String, FormatError> {
        Ok(String::from_utf8_lossy(self.take(len)?).into_owned())
    }

    fn value(&mut self, depth: usize) -> Result<Decoded, FormatError> {
        if depth > MAX_DEPTH {
            return Err(FormatError::Unsupported("AMF0 nesting depth"));
        }
        Ok(match self.u8()? {
            NUMBER => Decoded::Number(self.f64()?),
            BOOLEAN => Decoded::Bool(self.u8()? != 0),
            STRING => {
                let len = self.u16()?;
                Decoded::String(self.text(len)?)
            }
            LONG_STRING => {
                let len = self.u32()?;
                Decoded::String(self.text(len)?)
            }
            OBJECT => Decoded::Object(self.properties(depth)?),
            ECMA_ARRAY => {
                // The count is a hint only; the end marker terminates the array.
                self.u32()?;
                Decoded::Object(self.properties(depth)?)
            }
            STRICT_ARRAY => {
                let count = self.u32()?;
                // Every element takes at least one byte: bound the capacity by the input.
                let mut items = Vec::with_capacity(count.min(self.data.len() - self.pos));
                for _ in 0..count {
                    items.push(self.value(depth + 1)?);
                }
                Decoded::Array(items)
            }
            DATE => {
                let ms = self.f64()?;
                self.take(2)?;
                Decoded::Date(ms)
            }
            NULL => Decoded::Null,
            UNDEFINED => Decoded::Undefined,
            _ => return Err(FormatError::Unsupported("AMF0 type marker")),
        })
    }

    fn properties(&mut self, depth: usize) -> Result<Vec<(String, Decoded)>, FormatError> {
        let mut entries = Vec::new();
        loop {
            let len = self.u16()?;
            if len == 0 {
                if self.u8()? == OBJECT_END {
                    return Ok(entries);
                }
                return Err(FormatError::Invalid("AMF0 object end"));
            }
            let name = self.text(len)?;
            let v = self.value(depth + 1)?;
            entries.push((name, v));
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    #[test]
    fn encodes_known_bytes() {
        let mut out = Vec::new();
        string(&mut out, "onMetaData");
        assert_eq!(&out[..3], &[0x02, 0, 10]);
        let mut out = Vec::new();
        ecma_array(
            &mut out,
            &[("w", Value::Number(1.0)), ("s", Value::Bool(true))],
        );
        assert_eq!(
            out,
            [
                0x08, 0, 0, 0, 2, 0, 1, b'w', 0, 0x3f, 0xf0, 0, 0, 0, 0, 0, 0, 0, 1, b's', 1, 1, 0,
                0, 9
            ]
        );
        let mut out = Vec::new();
        object(&mut out, &[("a", Value::String("b")), ("n", Value::Null)]);
        assert_eq!(
            out,
            [
                0x03, 0, 1, b'a', 0x02, 0, 1, b'b', 0, 1, b'n', 0x05, 0, 0, 9
            ]
        );
    }

    #[test]
    fn long_strings_and_keys() {
        let long = "x".repeat(70_000);
        let mut out = Vec::new();
        string(&mut out, &long);
        assert_eq!(out[0], LONG_STRING);
        assert_eq!(decode(&out).unwrap().0.as_str(), Some(long.as_str()));
        // A key is cut at a character boundary below 65 535 bytes.
        let wide = "é".repeat(40_000);
        let mut out = Vec::new();
        key(&mut out, &wide);
        let len = usize::from(u16::from_be_bytes([out[0], out[1]]));
        assert_eq!(len % 2, 0);
        assert!(std::str::from_utf8(&out[2..]).is_ok());
    }

    #[test]
    fn round_trips_a_command() {
        let mut out = Vec::new();
        string(&mut out, "_result");
        number(&mut out, 1.0);
        object(
            &mut out,
            &[
                ("fmsVer", Value::String("FMS/3,0")),
                ("ok", Value::Bool(true)),
            ],
        );
        null(&mut out);
        let values = decode_all(&out).unwrap();
        assert_eq!(values.len(), 4);
        assert_eq!(values[0].as_str(), Some("_result"));
        assert_eq!(values[1].as_number(), Some(1.0));
        assert_eq!(
            values[2].get("fmsVer").and_then(Decoded::as_str),
            Some("FMS/3,0")
        );
        assert_eq!(values[2].get("ok"), Some(&Decoded::Bool(true)));
        assert_eq!(values[3], Decoded::Null);
    }

    #[test]
    fn decodes_arrays_dates_and_undefined() {
        let mut out = vec![STRICT_ARRAY, 0, 0, 0, 2];
        number(&mut out, 2.0);
        out.push(UNDEFINED);
        out.push(DATE);
        out.extend_from_slice(&5.0f64.to_be_bytes());
        out.extend_from_slice(&[0, 0]);
        let values = decode_all(&out).unwrap();
        assert_eq!(
            values,
            [
                Decoded::Array(vec![Decoded::Number(2.0), Decoded::Undefined]),
                Decoded::Date(5.0)
            ]
        );
    }

    #[test]
    fn refuses_bad_input() {
        // Every proper prefix of a valid value is truncated.
        let mut out = Vec::new();
        object(&mut out, &[("k", Value::String("v"))]);
        for end in 0..out.len() {
            assert!(decode(&out[..end]).is_err(), "prefix {end}");
        }
        assert_eq!(
            decode(&[0x0d]),
            Err(FormatError::Unsupported("AMF0 type marker"))
        );
        assert_eq!(
            decode(&[OBJECT, 0, 0, 0x07]),
            Err(FormatError::Invalid("AMF0 object end"))
        );
        // A huge strict-array count does not allocate for it.
        assert!(decode(&[STRICT_ARRAY, 0xff, 0xff, 0xff, 0xff]).is_err());
        // Nesting is bounded.
        let deep = [STRICT_ARRAY, 0, 0, 0, 1].repeat(MAX_DEPTH + 2);
        assert_eq!(
            decode(&deep),
            Err(FormatError::Unsupported("AMF0 nesting depth"))
        );
    }
}
