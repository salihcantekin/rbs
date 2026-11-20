//! Why a command id does not parse, and why a command cannot run now.

use std::borrow::Cow;
use std::fmt;

/// Why a command id is not valid.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandIdError(pub String);

impl fmt::Display for CommandIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CommandIdError {}

/// Why a command cannot run now ([`CommandState::enabled`](super::CommandState::enabled)):
/// one sentence the menu entry, toolbar button or palette row shows as its tooltip.
/// Fixed reasons are kept without a copy; a reason that names something owns its text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Disabled(Cow<'static, str>);

impl Disabled {
    /// A reason, fixed (`&'static str`) or built (`String`).
    pub fn new(reason: impl Into<Cow<'static, str>>) -> Disabled {
        Disabled(reason.into())
    }

    /// The reason as shown.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The reason as owned text, for a toolkit string or an error message.
    pub fn into_string(self) -> String {
        self.0.into_owned()
    }
}

impl From<&'static str> for Disabled {
    fn from(reason: &'static str) -> Disabled {
        Disabled(Cow::Borrowed(reason))
    }
}

impl From<String> for Disabled {
    fn from(reason: String) -> Disabled {
        Disabled(Cow::Owned(reason))
    }
}

/// A command id that does not parse cannot run; its parse error is the reason.
impl From<CommandIdError> for Disabled {
    fn from(e: CommandIdError) -> Disabled {
        Disabled(Cow::Owned(e.0))
    }
}

/// The reason reads as text (`starts_with`, `contains`) where it is checked.
impl std::ops::Deref for Disabled {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0
    }
}

impl PartialEq<str> for Disabled {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for Disabled {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl From<Disabled> for String {
    fn from(d: Disabled) -> String {
        d.into_string()
    }
}

impl fmt::Display for Disabled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Disabled {}
