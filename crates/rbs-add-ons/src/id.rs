//! Add-on ids.

use std::fmt;

use crate::catalog::CatalogError;

/// A stable add-on id in reverse-DNS form, such as `org.rbs.browser-engine`.
///
/// 3 to 64 bytes of `a-z`, `0-9`, `.` and `-`, with at least one dot, no empty
/// label, and no label that starts or ends with `-`. Ids are never reused; the id is
/// also the add-on's directory name, so these rules keep it a safe path component.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AddOnId(String);

impl AddOnId {
    /// Checks `id` against the rules above.
    pub fn new(id: &str) -> Result<Self, CatalogError> {
        let bad = |why: &str| Err(CatalogError::Invalid(format!("add-on id `{id}`: {why}")));
        if !(3..=64).contains(&id.len()) {
            return bad("must be 3 to 64 bytes");
        }
        if !id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-')
        {
            return bad("only a-z, 0-9, `.` and `-` are allowed");
        }
        if !id.contains('.') {
            return bad("must have a dot");
        }
        if id
            .split('.')
            .any(|label| label.is_empty() || label.starts_with('-') || label.ends_with('-'))
        {
            return bad("labels must be non-empty and must not start or end with `-`");
        }
        Ok(Self(id.to_owned()))
    }

    /// The id as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AddOnId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::AddOnId;

    #[test]
    fn accepts_reverse_dns_ids() {
        assert!(AddOnId::new("org.rbs.browser-engine").is_ok());
        assert!(AddOnId::new("a.b").is_ok());
    }

    #[test]
    fn refuses_unsafe_or_malformed_ids() {
        for id in [
            "ab", "nodot", "Org.rbs", "org..rbs", ".org.rbs", "org.rbs.", "org.-rbs", "org/rbs",
            "org\\rbs", "org.rb s",
        ] {
            assert!(AddOnId::new(id).is_err(), "{id}");
        }
        assert!(AddOnId::new(&format!("a.{}", "b".repeat(63))).is_err());
    }
}
