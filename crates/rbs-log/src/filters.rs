//! Setting levels: the only place that also keeps the `log` crate's filter in step
//! (ADR-0158 decision 7).

use std::fmt;

use rbs_core::log::{self, Area, Filter};

/// Sets `area`'s filter. For [`Area::Lib`] it also sets the `log` crate's maximum
/// level to the same, with Trace capped at Debug.
pub fn set_filter(area: Area, filter: Filter) {
    let filter = if area == Area::Lib && filter == Filter::Trace {
        Filter::Debug
    } else {
        filter
    };
    log::set_filter(area, filter);
    if area == Area::Lib {
        ::log::set_max_level(match filter {
            Filter::Off => ::log::LevelFilter::Off,
            Filter::Error => ::log::LevelFilter::Error,
            Filter::Warn => ::log::LevelFilter::Warn,
            Filter::Info => ::log::LevelFilter::Info,
            _ => ::log::LevelFilter::Debug,
        });
    }
}

/// The Settings level: every area but [`Area::Lib`], which stays as it is (a library
/// may log per frame at Debug).
pub fn apply_level(filter: Filter) {
    for area in Area::ALL {
        if area != Area::Lib {
            set_filter(area, filter);
        }
    }
}

/// `RBS_LOG` text that could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError(pub String);

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "cannot read the log level {:?}", self.0)
    }
}

impl std::error::Error for ParseError {}

/// The items of `RBS_LOG` text: `debug` (every area, `None`) or `stream=trace`, comma
/// separated, later items winning when applied in order.
pub fn parse_filters(text: &str) -> Result<Vec<(Option<Area>, Filter)>, ParseError> {
    let mut out = Vec::new();
    for item in text.split(',').map(str::trim).filter(|i| !i.is_empty()) {
        let bad = || ParseError(item.to_owned());
        match item.split_once('=') {
            Some((area, level)) => {
                let area = Area::from_name(area.trim()).ok_or_else(bad)?;
                let level = Filter::from_name(level.trim()).ok_or_else(bad)?;
                out.push((Some(area), level));
            }
            None => out.push((None, Filter::from_name(item).ok_or_else(bad)?)),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rbs_log_text_is_read() {
        assert_eq!(
            parse_filters(" debug , stream=TRACE,lib=warn,"),
            Ok(vec![
                (None, Filter::Debug),
                (Some(Area::Stream), Filter::Trace),
                (Some(Area::Lib), Filter::Warn),
            ])
        );
        assert_eq!(parse_filters(""), Ok(vec![]));
        assert_eq!(parse_filters("loud"), Err(ParseError("loud".into())));
        assert_eq!(
            parse_filters("nope=info"),
            Err(ParseError("nope=info".into()))
        );
        assert_eq!(parse_filters("ui=loud"), Err(ParseError("ui=loud".into())));
    }
}
