//! Which output profile and scene collection are in use: `current.toml` in the config
//! directory.

use serde::{Deserialize, Serialize};

use crate::document::{self, Document};
use crate::library::EntryId;

/// The entries in use. An id that no longer exists is treated as unset.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Current {
    /// The output profile in use.
    pub profile: Option<EntryId>,
    /// The scene collection in use.
    pub scene_collection: Option<EntryId>,
}

impl Current {
    /// The given entries.
    pub fn new(profile: Option<EntryId>, scene_collection: Option<EntryId>) -> Self {
        Self {
            profile,
            scene_collection,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Body {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scene_collection: Option<String>,
}

impl Document for Current {
    const KIND: &'static str = "current";
    const FORMAT: u32 = 1;

    fn from_body(body: toml::Table) -> Result<Self, String> {
        let b: Body = document::from_body(body)?;
        let id = |v: Option<String>, what: &str| {
            v.map(|s| EntryId::parse(&s).ok_or_else(|| format!("{what} {s:?} is not an id")))
                .transpose()
        };
        Ok(Self {
            profile: id(b.profile, "profile")?,
            scene_collection: id(b.scene_collection, "scene collection")?,
        })
    }

    fn to_body(&self) -> Result<toml::Table, String> {
        document::body_of(&Body {
            profile: self.profile.as_ref().map(|i| i.as_str().to_owned()),
            scene_collection: self
                .scene_collection
                .as_ref()
                .map(|i| i.as_str().to_owned()),
        })
    }
}
