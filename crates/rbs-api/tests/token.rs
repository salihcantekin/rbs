//! Tokens (ADR-0044 decision 9): format, the spec's test vector, storage and lookup.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use rbs_api::{ScopeSet, Token, TokenEntry, TokenHash, TokenStore};

#[test]
fn the_spec_test_vector_hashes_as_documented() {
    let bytes: [u8; 32] = std::array::from_fn(|i| i as u8);
    let token = Token::from_bytes(&bytes);
    assert_eq!(
        token.expose(),
        "rbs1_000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f"
    );
    assert_eq!(
        token.hash().to_hex(),
        "73aa00dce5b7afd18306d160c3de0e6e56db9ade3300340da67c2021c402f04b"
    );
}

#[test]
fn generated_tokens_have_the_format_and_differ() {
    let a = Token::generate().unwrap();
    let b = Token::generate().unwrap();
    assert_ne!(a, b);
    assert_eq!(a.expose().len(), 5 + 64);
    assert_eq!(Token::parse(a.expose()), Some(a.clone()));
    assert_eq!(format!("{a:?}"), "Token(<redacted>)");
}

#[test]
fn parse_refuses_other_text() {
    let ok = "rbs1_".to_owned() + &"ab".repeat(32);
    assert!(Token::parse(&ok).is_some());
    assert!(Token::parse(&ok.to_uppercase()).is_none());
    assert!(Token::parse(&ok[..ok.len() - 1]).is_none());
    assert!(Token::parse(&ok.replace("rbs1_", "rbs2_")).is_none());
    assert!(Token::parse("").is_none());
}

#[test]
fn the_store_finds_by_hash_and_revokes_by_name() {
    let a = Token::from_bytes(&[1; 32]);
    let b = Token::from_bytes(&[2; 32]);
    let mut store = TokenStore::new();
    for (t, name) in [(&a, "Deck"), (&b, "Script")] {
        store.insert(TokenEntry {
            name: name.into(),
            scopes: ScopeSet::PANEL,
            hash: t.hash(),
        });
    }
    assert_eq!(store.verify(b.expose()).unwrap().name, "Script");
    assert_eq!(store.verify(a.expose()).unwrap().name, "Deck");
    assert!(store.verify("rbs1_nope").is_none());
    assert!(store.revoke("Deck"));
    assert!(store.verify(a.expose()).is_none());
    assert!(!store.revoke("Deck"));
}

#[test]
fn the_store_serializes_hashes_only() {
    let t = Token::from_bytes(&[7; 32]);
    let mut store = TokenStore::new();
    store.insert(TokenEntry {
        name: "Deck".into(),
        scopes: ScopeSet::PANEL,
        hash: t.hash(),
    });
    let json = serde_json::to_string(&store).unwrap();
    assert!(!json.contains(t.expose()));
    assert!(json.contains(&t.hash().to_hex()));
    let back: TokenStore = serde_json::from_str(&json).unwrap();
    assert_eq!(back, store);
    assert!(TokenHash::from_hex("zz").is_none());
}
