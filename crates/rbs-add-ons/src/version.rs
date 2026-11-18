//! Add-on versions: dotted numbers.

use std::cmp::Ordering;
use std::fmt;

/// A version made of 1 to 6 dot-separated decimal numbers, such as `156.6.0`.
///
/// Versions order by their numbers (`156.6.0 < 156.10.0`); a missing trailing part
/// counts as zero for ordering. The text is kept as written because it is also the
/// version's directory name. Text that does not parse is never a `Version`, and
/// such directories are never ordered or removed by `clean` (ADR-0145 decision 4).
#[derive(Debug, Clone)]
pub struct Version {
    parts: Vec<u32>,
    text: String,
}

impl Version {
    /// Parses `text`; `None` when it is not 1 to 6 dot-separated decimal numbers
    /// (no sign, no leading `+`, at most 9 digits each).
    pub fn parse(text: &str) -> Option<Self> {
        let parts = text
            .split('.')
            .map(|part| {
                if part.is_empty() || part.len() > 9 || !part.bytes().all(|b| b.is_ascii_digit()) {
                    None
                } else {
                    part.parse::<u32>().ok()
                }
            })
            .collect::<Option<Vec<u32>>>()?;
        if parts.len() > 6 {
            return None;
        }
        Some(Self {
            parts,
            text: text.to_owned(),
        })
    }

    /// The version as written.
    pub fn as_str(&self) -> &str {
        &self.text
    }

    fn part(&self, index: usize) -> u32 {
        self.parts.get(index).copied().unwrap_or(0)
    }
}

impl PartialEq for Version {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Version {}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        let len = self.parts.len().max(other.parts.len());
        (0..len)
            .map(|i| self.part(i).cmp(&other.part(i)))
            .find(|o| *o != Ordering::Equal)
            .unwrap_or(Ordering::Equal)
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

#[cfg(test)]
mod tests {
    use super::Version;

    fn v(text: &str) -> Version {
        Version::parse(text).unwrap_or_else(|| panic!("{text} should parse"))
    }

    #[test]
    fn orders_by_number_not_text() {
        assert!(v("156.6.0") < v("156.10.0"));
        assert!(v("9") < v("10"));
        assert_eq!(v("1.0"), v("1.0.0"));
        assert!(v("1.0.1") > v("1"));
    }

    #[test]
    fn refuses_other_text() {
        for text in [
            "",
            "1.",
            ".1",
            "1..2",
            "v1",
            "1.2-beta",
            "+1",
            "1.2.3.4.5.6.7",
            "1234567890",
        ] {
            assert!(Version::parse(text).is_none(), "{text}");
        }
        assert_eq!(v("156.6.0").as_str(), "156.6.0");
    }
}
