//! `api.describe` loads schemas in parts (ADR-0196). Own test binary: the shard
//! counter is process-wide.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod support;

use rbs_api::request::METHODS;
use rbs_api::transport::PeerInfo;
use serde_json::json;
use support::*;

// One test: the shard counter is process-wide and the second half parses every shard.
#[test]
fn shards_parse_lazily_and_match_the_method_namespaces() {
    let mut h = Harness::new();
    let conn = h.login(PeerInfo::loopback_websocket(), PANEL_TOKEN_SEED);
    assert_eq!(rbs_api::schema::parsed_shards(), 0);
    h.call(conn, "api.describe", json!({}));
    h.one(conn);
    assert_eq!(
        rbs_api::schema::parsed_shards(),
        0,
        "an index parses nothing"
    );
    h.call(
        conn,
        "api.describe",
        json!({"detail": "schemas", "methods": ["scene.list"]}),
    );
    h.one(conn);
    assert_eq!(rbs_api::schema::parsed_shards(), 1);
    h.call(
        conn,
        "api.describe",
        json!({"detail": "schemas", "methods": ["scene.list", "scene.add"]}),
    );
    h.one(conn);
    assert_eq!(
        rbs_api::schema::parsed_shards(),
        1,
        "a shard is parsed once"
    );

    let in_methods: std::collections::BTreeSet<&str> = METHODS
        .iter()
        .map(|m| m.name.split('.').next().unwrap())
        .collect();
    let shards: std::collections::BTreeSet<&str> = rbs_api::schema::shard_namespaces().collect();
    assert_eq!(shards, in_methods);
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("schemas");
    let files: std::collections::BTreeSet<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    let expected: std::collections::BTreeSet<String> =
        shards.iter().map(|ns| format!("{ns}.json")).collect();
    assert_eq!(files, expected);
    for ns in shards {
        for name in rbs_api::schema::namespace_schemas(ns)
            .unwrap()
            .unwrap()
            .keys()
        {
            assert_eq!(
                name.split('.').next().unwrap(),
                ns,
                "{name} is in the wrong shard"
            );
        }
    }
}
