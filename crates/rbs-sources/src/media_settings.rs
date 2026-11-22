//! The settings of a media file source (ADR-0108 decisions 11 and 12): checking them,
//! the play range, and the live keys sent to the control handle. Moved out of
//! `registry.rs` unchanged.

use std::time::Duration;

use rbs_scene::model::{SettingValue, Settings};

use crate::registry::{SourceError, flag, text};
use crate::types::keys;

pub(crate) fn validate_media(settings: &Settings) -> Result<(), SourceError> {
    for key in settings.keys() {
        if ![
            keys::ADAPTER,
            keys::PATH,
            keys::LOOP,
            keys::RESTART_WHEN_SHOWN,
            keys::START_MS,
            keys::END_MS,
        ]
        .contains(&key.as_str())
        {
            return Err(SourceError::InvalidSetting {
                key: "settings",
                expected: "only keys this source type uses",
            });
        }
    }
    match settings.get(keys::PATH) {
        Some(SettingValue::Text(path)) => {
            // Shape only here. `instantiate` canonicalizes before any decoder open.
            if rbs_media_decode::accept_local_media_path(path).is_err() && raw_path_is_remote(path)
            {
                return Err(SourceError::InvalidSetting {
                    key: keys::PATH,
                    expected: "a local absolute path, not a URL, UNC path, or device path",
                });
            }
        }
        Some(_) => {
            return Err(SourceError::InvalidSetting {
                key: keys::PATH,
                expected: "text",
            });
        }
        None => {
            return Err(SourceError::InvalidSetting {
                key: keys::PATH,
                expected: "a local absolute path",
            });
        }
    }
    flag(settings, keys::LOOP)?;
    flag(settings, keys::RESTART_WHEN_SHOWN)?;
    media_range(settings)?;
    text(settings, keys::ADAPTER)?;
    Ok(())
}

/// Sends the live keys `keys` of a media file's `settings` to its control handle
/// (ADR-0108 decision 12), for [`rbs_scene::binder::BindReport::live_changed`]. A
/// value the type refuses is skipped and reported; the others still apply.
pub fn apply_media_live(
    control: &rbs_nodes::MediaControl,
    settings: &Settings,
    keys: &[String],
) -> Result<(), SourceError> {
    let mut first_error = None;
    let mut keep = |result: Result<(), SourceError>| {
        if let Err(error) = result {
            first_error.get_or_insert(error);
        }
    };
    let changed = |key: &str| keys.iter().any(|k| k == key);
    // `restart_when_shown` is the deprecated spelling of the activity policy's
    // `on_return` (ADR-0152 decision 10): the host maps it and delivers the policy
    // through the binder's level changes, so a stray value only has to be valid.
    if changed(keys::RESTART_WHEN_SHOWN) {
        keep(flag(settings, keys::RESTART_WHEN_SHOWN).map(|_| ()));
    }
    if changed(keys::START_MS) || changed(keys::END_MS) {
        keep(media_range(settings).and_then(|(start, end)| {
            control
                .set_range(start, end)
                .map_err(|_| SourceError::InvalidSetting {
                    key: keys::END_MS,
                    expected: "greater than start_ms",
                })
        }));
    }
    // Last, so an end hold that loops again starts from the new range.
    if changed(keys::LOOP) {
        keep(flag(settings, keys::LOOP).and_then(|on| {
            control
                .set_loop(on.unwrap_or(true))
                .map_err(|error| SourceError::Media(error.to_string()))
        }));
    }
    first_error.map_or(Ok(()), Err)
}

/// The play range of a media file (ADR-0108 decision 11): `start_ms` (default 0) and
/// `end_ms` (absent for the end of the file), each a whole number of milliseconds of
/// at least 0, and `end_ms` greater than `start_ms`. Values past the end of the file
/// are clamped when it opens.
pub(crate) fn media_range(
    settings: &Settings,
) -> Result<(Duration, Option<Duration>), SourceError> {
    let ms = |key: &'static str| match settings.get(key) {
        None => Ok(None),
        Some(SettingValue::Int(value)) => u64::try_from(*value)
            .map(|value| Some(Duration::from_millis(value)))
            .map_err(|_| SourceError::InvalidSetting {
                key,
                expected: "milliseconds, 0 or more",
            }),
        Some(_) => Err(SourceError::InvalidSetting {
            key,
            expected: "an integer",
        }),
    };
    let start = ms(keys::START_MS)?.unwrap_or(Duration::ZERO);
    let end = ms(keys::END_MS)?;
    if end.is_some_and(|end| end <= start) {
        return Err(SourceError::InvalidSetting {
            key: keys::END_MS,
            expected: "greater than start_ms",
        });
    }
    Ok((start, end))
}

fn raw_path_is_remote(path: &str) -> bool {
    let lower = path.trim().to_ascii_lowercase();
    lower.contains("://")
        || path.trim().starts_with(r"\\")
        || path.trim().starts_with("//")
        || lower.starts_with("/dev/")
        || lower.starts_with("/dev\\")
}
