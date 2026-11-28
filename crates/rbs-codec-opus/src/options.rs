//! Parsing of the encoder's `adapter_options` (opaque to the engine, principle P3).

use rbs_codec::CreateError;

use crate::ffi::Application;

/// Validated encoder options.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EncoderOptions {
    pub application: Application,
    /// Sample frames per channel in one packet at 48 kHz.
    pub frame: usize,
    pub complexity: i32,
    pub vbr: bool,
    pub fec: bool,
    pub loss_percent: i32,
}

impl Default for EncoderOptions {
    fn default() -> Self {
        Self {
            application: Application::Audio,
            frame: 960,
            complexity: 9,
            vbr: true,
            fec: false,
            loss_percent: 0,
        }
    }
}

fn bad(key: &str, value: &str) -> CreateError {
    CreateError::Unsupported {
        what: format!("{key} {value}"),
    }
}

fn on_off(key: &str, value: &str) -> Result<bool, CreateError> {
    match value {
        "on" => Ok(true),
        "off" => Ok(false),
        _ => Err(bad(key, value)),
    }
}

fn ranged(key: &str, value: &str, max: i32) -> Result<i32, CreateError> {
    value
        .parse::<i32>()
        .ok()
        .filter(|v| (0..=max).contains(v))
        .ok_or_else(|| bad(key, value))
}

impl EncoderOptions {
    /// Parses `options`; an unknown key or value is refused, never ignored.
    pub fn parse(options: &[(String, String)]) -> Result<Self, CreateError> {
        let mut parsed = Self::default();
        for (key, value) in options {
            match key.as_str() {
                "application" => {
                    parsed.application = match value.as_str() {
                        "audio" => Application::Audio,
                        "voip" => Application::Voip,
                        "lowdelay" => Application::LowDelay,
                        _ => return Err(bad(key, value)),
                    }
                }
                "frame_ms" => {
                    parsed.frame = match value.as_str() {
                        "2.5" => 120,
                        "5" => 240,
                        "10" => 480,
                        "20" => 960,
                        "40" => 1920,
                        "60" => 2880,
                        _ => return Err(bad(key, value)),
                    }
                }
                "complexity" => parsed.complexity = ranged(key, value, 10)?,
                "vbr" => parsed.vbr = on_off(key, value)?,
                "fec" => parsed.fec = on_off(key, value)?,
                "loss_percent" => parsed.loss_percent = ranged(key, value, 100)?,
                _ => {
                    return Err(CreateError::Unsupported {
                        what: format!("adapter option {key}"),
                    });
                }
            }
        }
        Ok(parsed)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn opts(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn defaults_are_20_ms_audio_vbr() {
        let o = EncoderOptions::parse(&[]).unwrap();
        assert_eq!(o.frame, 960);
        assert_eq!(o.application, Application::Audio);
        assert!(o.vbr && !o.fec);
    }

    #[test]
    fn known_options_parse_and_unknown_ones_are_refused() {
        let o = EncoderOptions::parse(&opts(&[
            ("frame_ms", "2.5"),
            ("application", "voip"),
            ("fec", "on"),
            ("loss_percent", "15"),
            ("vbr", "off"),
            ("complexity", "3"),
        ]))
        .unwrap();
        assert_eq!((o.frame, o.loss_percent, o.complexity), (120, 15, 3));
        assert!(o.fec && !o.vbr && o.application == Application::Voip);
        for bad in [
            ("frame_ms", "25"),
            ("complexity", "11"),
            ("loss_percent", "101"),
            ("fec", "yes"),
            ("bogus", "1"),
        ] {
            assert!(
                matches!(
                    EncoderOptions::parse(&opts(&[bad])),
                    Err(CreateError::Unsupported { .. })
                ),
                "{bad:?}"
            );
        }
    }
}
