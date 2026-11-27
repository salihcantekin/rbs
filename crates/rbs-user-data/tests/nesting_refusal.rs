//! ADR-0188 document refusal and preservation.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use rbs_scene::{SceneCollection, SceneError};
use rbs_user_data::{BodyError, DataError, document};

fn nested(format: u32) -> String {
    format!(
        "format = {format}\nkind = \"scene-collection\"\n\
         [[scenes]]\nid = 1\nname = \"Parent\"\n\
         [[scenes.items]]\nid = 1\nscene = 1\n\
         [scenes.items.layout]\nposition = [0.0, 0.0]\nsize = [1.0, 1.0]\n"
    )
}

#[test]
fn supported_formats_refuse_nested_items_before_upgrades() {
    for version in [1, 15, 30, 31] {
        let err = document::parse::<SceneCollection>(&nested(version)).unwrap_err();
        assert!(matches!(
            err,
            DataError::Invalid(BodyError::Scene(SceneError::NestingUnsupported))
        ));
        assert_eq!(
            err.to_string(),
            "Scenes cannot be placed inside other scenes."
        );
    }
}

#[test]
fn write_and_known_writer_preserve_existing_nested_document() {
    let dir = std::env::current_dir()
        .unwrap()
        .join(format!("nesting-refusal-{}", std::process::id()));
    std::fs::create_dir(&dir).unwrap();
    let path = dir.join("collection.toml");
    let old = nested(1);
    std::fs::write(&path, &old).unwrap();
    let document = SceneCollection::new();
    let mut written = document::Written::default();
    for result in [
        document::write(&path, &document, None),
        document::write_known(&path, &document, None, &mut written),
    ] {
        assert!(matches!(
            result,
            Err(DataError::Invalid(BodyError::Scene(
                SceneError::NestingUnsupported
            )))
        ));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), old);
    }
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
    std::fs::remove_dir_all(dir).unwrap();
}
