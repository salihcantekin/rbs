//! Bearer tokens (ADR-0044 decision 9): `rbs1_` followed by 64 lowercase hex digits of
//! OS randomness. The server stores only SHA-256 of the token's text and compares in
//! constant time, so a stolen store does not reveal tokens.

use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::{ConditionallySelectable, ConstantTimeEq};

use crate::scope::ScopeSet;

/// The prefix of every token of this format.
pub const TOKEN_PREFIX: &str = "rbs1_";

/// Random bytes in a token.
const TOKEN_BYTES: usize = 32;

/// A token's secret text. Shown to the user once at creation; never logged: its
/// `Debug` output is redacted.
#[derive(Clone, PartialEq, Eq)]
pub struct Token(String);

impl Token {
    /// A new token from OS randomness.
    pub fn generate() -> Result<Token, getrandom::Error> {
        let mut bytes = [0u8; TOKEN_BYTES];
        getrandom::fill(&mut bytes)?;
        Ok(Token::from_bytes(&bytes))
    }

    /// The token of these random bytes (tests and the test vector).
    pub fn from_bytes(bytes: &[u8; TOKEN_BYTES]) -> Token {
        let mut text = String::with_capacity(TOKEN_PREFIX.len() + 2 * TOKEN_BYTES);
        text.push_str(TOKEN_PREFIX);
        push_hex(&mut text, bytes);
        Token(text)
    }

    /// Reads a token in this format, or `None` when `text` is not one.
    pub fn parse(text: &str) -> Option<Token> {
        let hex = text.strip_prefix(TOKEN_PREFIX)?;
        let ok = hex.len() == 2 * TOKEN_BYTES
            && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        ok.then(|| Token(text.to_owned()))
    }

    /// The text the user copies.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// What the server stores for it.
    pub fn hash(&self) -> TokenHash {
        TokenHash::of(&self.0)
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

/// SHA-256 of a token's UTF-8 text. Stored and shown as 64 lowercase hex digits.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct TokenHash([u8; 32]);

impl TokenHash {
    /// The hash of any presented text, valid token or not, so a malformed token takes
    /// the same path as a wrong one.
    pub fn of(text: &str) -> TokenHash {
        TokenHash(Sha256::digest(text.as_bytes()).into())
    }

    /// Reads 64 lowercase hex digits.
    pub fn from_hex(hex: &str) -> Option<TokenHash> {
        let bytes = hex.as_bytes();
        if bytes.len() != 64 {
            return None;
        }
        let mut out = [0u8; 32];
        for (i, pair) in bytes.chunks_exact(2).enumerate() {
            out[i] = (nibble(pair[0])? << 4) | nibble(pair[1])?;
        }
        Some(TokenHash(out))
    }

    /// 64 lowercase hex digits.
    pub fn to_hex(&self) -> String {
        let mut s = String::with_capacity(64);
        push_hex(&mut s, &self.0);
        s
    }
}

impl ConstantTimeEq for TokenHash {
    fn ct_eq(&self, other: &Self) -> subtle::Choice {
        self.0.ct_eq(&other.0)
    }
}

impl fmt::Debug for TokenHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TokenHash({})", self.to_hex())
    }
}

impl Serialize for TokenHash {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for TokenHash {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        TokenHash::from_hex(&s)
            .ok_or_else(|| serde::de::Error::custom("expected 64 lowercase hex digits"))
    }
}

/// One stored token: a user-given name, its scopes and its hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenEntry {
    /// The name the user gave it ("Stream Deck"); logged with its sessions' calls.
    pub name: String,
    /// What its sessions may do (before channel limits).
    pub scopes: ScopeSet,
    /// SHA-256 of the token.
    pub hash: TokenHash,
}

/// The tokens the server accepts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenStore {
    entries: Vec<TokenEntry>,
}

impl TokenStore {
    /// An empty store: only same-user IPC peers can connect.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a token.
    pub fn insert(&mut self, entry: TokenEntry) {
        self.entries.push(entry);
    }

    /// Removes every token named `name`; true when one was removed.
    pub fn revoke(&mut self, name: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| e.name != name);
        self.entries.len() != before
    }

    /// The stored tokens.
    pub fn entries(&self) -> &[TokenEntry] {
        &self.entries
    }

    /// True when a token with this hash is stored.
    pub fn contains(&self, hash: &TokenHash) -> bool {
        self.entries.iter().any(|e| e.hash == *hash)
    }

    /// The entry of the presented text, or `None`. Hashes the text whatever it is and
    /// compares with every entry in constant time, without stopping early, so the time
    /// taken does not depend on whether or where the token is stored.
    pub fn verify(&self, presented: &str) -> Option<&TokenEntry> {
        let hash = TokenHash::of(presented);
        let mut found = subtle::Choice::from(0);
        let mut index = 0u32;
        for (i, e) in self.entries.iter().enumerate() {
            let hit = e.hash.ct_eq(&hash);
            let i = u32::try_from(i).unwrap_or(u32::MAX);
            index = u32::conditional_select(&index, &i, hit & !found);
            found |= hit;
        }
        if bool::from(found) {
            self.entries.get(usize::try_from(index).ok()?)
        } else {
            None
        }
    }
}

fn push_hex(out: &mut String, bytes: &[u8]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for b in bytes {
        out.push(char::from(HEX[usize::from(b >> 4)]));
        out.push(char::from(HEX[usize::from(b & 0x0f)]));
    }
}

fn nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    }
}
