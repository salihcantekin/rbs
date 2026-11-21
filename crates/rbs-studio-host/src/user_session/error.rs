//! Why a user-data step of the session was refused or failed (quality plan WP-25). The
//! text of each kind is the one shown to the user.

use std::fmt;

use rbs_user_data::DataError;

/// Why a profile, collection, theme or shortcut step did not happen.
#[non_exhaustive]
#[derive(Debug)]
pub enum SessionError {
    /// The user data refused it or could not be read or written.
    Data(DataError),
    /// Another RBS holds the user data, so this one saves nothing.
    ReadOnly,
    /// The writer thread is gone (the app is closing).
    WriterStopped,
    /// The output profile in use cannot be deleted.
    ProfileInUse,
    /// The scene collection in use cannot be deleted.
    CollectionInUse,
    /// The section was saved by a newer RBS and is kept as it is.
    NewerSection,
    /// No new browser profile id could be made (the system's reason).
    NoProfileId(String),
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SessionError::Data(e) => e.fmt(f),
            SessionError::ReadOnly => f.write_str(
                "another RBS is running with the same settings, so changes here are not saved",
            ),
            SessionError::WriterStopped => f.write_str("the user data writer stopped"),
            SessionError::ProfileInUse => {
                f.write_str("the output profile in use cannot be deleted")
            }
            SessionError::CollectionInUse => {
                f.write_str("the scene collection in use cannot be deleted")
            }
            SessionError::NewerSection => {
                f.write_str("they were saved by a newer RBS, so this version does not change them")
            }
            SessionError::NoProfileId(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for SessionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SessionError::Data(e) => Some(e),
            _ => None,
        }
    }
}

/// The UI edge: a window shows the words.
impl From<SessionError> for String {
    fn from(e: SessionError) -> Self {
        e.to_string()
    }
}

impl From<DataError> for SessionError {
    fn from(e: DataError) -> Self {
        SessionError::Data(e)
    }
}
