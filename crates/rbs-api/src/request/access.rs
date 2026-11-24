//! Token and add-on requests.

use crate::scope::ScopeSet;
use serde::{Deserialize, Serialize};

/// `tokens.create`: a named access token for apps (ADR-0044 decision 9).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CreateToken {
    /// Its name, unique; surrounding spaces are dropped.
    pub name: String,
    /// What it allows: `["read"]`, `["read", "control"]` or `["read", "control",
    /// "admin"]` (the access levels the desktop app offers). Never more than the
    /// caller's own scopes (`not_permitted`, protocol 1.80).
    #[cfg_attr(feature = "schema", schemars(with = "Vec<crate::Scope>"))]
    pub scopes: ScopeSet,
    /// "Allow actions that cannot be undone" (protocol 1.80, ADR-0052 decision 4);
    /// absent: allowed, as tokens behave without the field. Only a caller that holds
    /// the permission reaches `tokens.create` (it is `destructive`), so it can give
    /// either.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub irreversible: Option<bool>,
}

/// `tokens.revoke`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TokenRef {
    /// The token's name.
    pub name: String,
}

/// `addon.install`, `addon.cancel`, `addon.clean` and `addon.remove` (protocol 1.90,
/// ADR-0145): an add-on of the built-in catalog, by id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct AddOnRef {
    /// The add-on's id, as `addon.list` names it (`org.rbs.browser-engine`).
    pub id: String,
}
