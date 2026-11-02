//! The few AMF0 values an `onMetaData` script tag needs.

/// Appends an AMF0 string value (marker 0x02).
pub fn string(out: &mut Vec<u8>, value: &str) {
    out.push(0x02);
    key(out, value);
}

/// Appends an object or ECMA-array key: a 16-bit length and UTF-8 bytes.
pub fn key(out: &mut Vec<u8>, value: &str) {
    let bytes = &value.as_bytes()[..value.len().min(usize::from(u16::MAX))];
    out.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
    out.extend_from_slice(bytes);
}

/// Appends an AMF0 number (marker 0x00, IEEE 754 double).
pub fn number(out: &mut Vec<u8>, value: f64) {
    out.push(0x00);
    out.extend_from_slice(&value.to_be_bytes());
}

/// Appends an AMF0 boolean (marker 0x01).
pub fn boolean(out: &mut Vec<u8>, value: bool) {
    out.extend_from_slice(&[0x01, u8::from(value)]);
}

/// Appends an ECMA array (marker 0x08) of `(key, number or bool)` entries.
pub fn ecma_array(out: &mut Vec<u8>, entries: &[(&str, Value)]) {
    out.push(0x08);
    out.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    for (name, value) in entries {
        key(out, name);
        match value {
            Value::Number(n) => number(out, *n),
            Value::Bool(b) => boolean(out, *b),
        }
    }
    // Object end: empty key, then marker 0x09.
    out.extend_from_slice(&[0, 0, 0x09]);
}

/// A metadata value.
#[derive(Debug, Clone, Copy)]
pub enum Value {
    /// AMF0 number.
    Number(f64),
    /// AMF0 boolean.
    Bool(bool),
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
    }
}
