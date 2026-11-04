//! Scopes: what an authenticated session may do (ADR-0044 decision 10).

use std::fmt;

use serde::{Deserialize, Serialize};

/// One permission. Every method declares exactly one scope (or none, for the
/// handshake); a call outside the session's scopes returns `scope_denied`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// Queries, `api.describe`, event subscriptions.
    Read,
    /// Scene and source changes, capture-target lists, record start and stop with
    /// configured profiles, undo and redo, `ui.*`.
    Control,
    /// Settings, explicit file paths. Loopback and IPC only, unless the user turned on
    /// "Allow remote admin".
    Admin,
    /// Plugin management. Loopback and IPC only.
    Plugins,
    /// Token management. Loopback and IPC only.
    Tokens,
}

impl Scope {
    /// Every scope, in declaration order.
    pub const ALL: [Scope; 5] = [
        Scope::Read,
        Scope::Control,
        Scope::Admin,
        Scope::Plugins,
        Scope::Tokens,
    ];

    /// Its name on the wire.
    pub const fn name(self) -> &'static str {
        match self {
            Scope::Read => "read",
            Scope::Control => "control",
            Scope::Admin => "admin",
            Scope::Plugins => "plugins",
            Scope::Tokens => "tokens",
        }
    }

    const fn bit(self) -> u8 {
        1 << self as u8
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A set of scopes. Serialized as a list of names.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct ScopeSet(u8);

impl ScopeSet {
    /// No scope.
    pub const EMPTY: ScopeSet = ScopeSet(0);
    /// Every scope: the in-process UI and same-user IPC peers.
    pub const ALL: ScopeSet = ScopeSet(0b1_1111);
    /// `read` + `control`: a hardware panel's token.
    pub const PANEL: ScopeSet = ScopeSet(Scope::Read.bit() | Scope::Control.bit());

    /// The set of `scopes`.
    pub fn of(scopes: impl IntoIterator<Item = Scope>) -> Self {
        scopes
            .into_iter()
            .fold(Self::EMPTY, |s, scope| s.with(scope))
    }

    /// This set plus `scope`.
    pub const fn with(self, scope: Scope) -> Self {
        Self(self.0 | scope.bit())
    }

    /// This set without `scope`.
    pub const fn without(self, scope: Scope) -> Self {
        Self(self.0 & !scope.bit())
    }

    /// True when `scope` is in the set.
    pub const fn contains(self, scope: Scope) -> bool {
        self.0 & scope.bit() != 0
    }

    /// True when the set is empty.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The scopes in the set, in declaration order.
    pub fn iter(self) -> impl Iterator<Item = Scope> {
        Scope::ALL.into_iter().filter(move |s| self.contains(*s))
    }

    /// The scopes a session gets over a channel: `plugins` and `tokens` only from
    /// loopback or IPC peers; `admin` only from them too, unless remote admin is
    /// allowed (ADR-0044 decision 10).
    pub const fn for_channel(self, local: bool, allow_remote_admin: bool) -> Self {
        if local {
            return self;
        }
        let s = self.without(Scope::Plugins).without(Scope::Tokens);
        if allow_remote_admin {
            s
        } else {
            s.without(Scope::Admin)
        }
    }
}

impl fmt::Debug for ScopeSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.iter()).finish()
    }
}

impl FromIterator<Scope> for ScopeSet {
    fn from_iter<T: IntoIterator<Item = Scope>>(iter: T) -> Self {
        Self::of(iter)
    }
}

impl Serialize for ScopeSet {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self.iter())
    }
}

impl<'de> Deserialize<'de> for ScopeSet {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Vec::<Scope>::deserialize(deserializer)?
            .into_iter()
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_channels_lose_local_only_scopes() {
        let all = ScopeSet::ALL;
        assert_eq!(all.for_channel(true, false), all);
        assert_eq!(all.for_channel(false, false), ScopeSet::PANEL);
        assert_eq!(
            all.for_channel(false, true),
            ScopeSet::PANEL.with(Scope::Admin)
        );
    }

    #[test]
    fn serializes_as_names() {
        let json = serde_json::to_string(&ScopeSet::PANEL).ok();
        assert_eq!(json.as_deref(), Some(r#"["read","control"]"#));
        let back: Option<ScopeSet> = serde_json::from_str(r#"["tokens","read"]"#).ok();
        assert_eq!(back, Some(ScopeSet::of([Scope::Read, Scope::Tokens])));
    }
}
