use super::*;
use rbs_scene::binder::OutgoingScene;

#[test]
fn manual_program_cues_are_frozen_per_occurrence_but_preview_and_edits_are_live() {
    let (mut s, _) = audio_setup();
    let (next, _) = s.col.add_scene("Next", SceneScope::Shared).unwrap();
    let add_media = |col: &mut SceneCollection, name| {
        col.add_source(name, ty("org.rbs.source.media"), Settings::new())
            .unwrap()
            .0
    };
    let expired = add_media(&mut s.col, "Expired");
    let late = add_media(&mut s.col, "Late");
    let shared = add_media(&mut s.col, "Shared");
    let mut add_hidden = |scene, source| {
        let (item, _) = s
            .col
            .add_item(scene, ItemContent::Source(source), full(), None)
            .unwrap();
        s.col.set_item_visible(item, false).unwrap();
        item
    };
    let expired_item = add_hidden(s.live, expired);
    let late_item = add_hidden(s.live, late);
    let shared_program = add_hidden(s.live, shared);
    let shared_preview = add_hidden(next, shared);
    s.col.set_preview(s.wide, Some(next)).unwrap();
    let outgoing = BTreeMap::from([(
        s.wide,
        OutgoingScene::towards(next).with_program_cues(BTreeSet::from([expired_item])),
    )]);
    let live = BTreeSet::from([late_item, shared_program, shared_preview]);
    let report = s
        .binder
        .sync_transition(
            &s.col,
            &mut s.graph,
            &live,
            &BTreeSet::new(),
            &BTreeMap::new(),
            &outgoing,
        )
        .unwrap();
    let program = &report.compositors[&(s.wide, Role::Program)].layers;
    assert!(program.iter().any(|l| l.source == expired));
    assert!(
        !program
            .iter()
            .any(|l| l.source == late || l.source == shared)
    );
    let incoming = &report.compositors[&(s.wide, Role::Incoming)].layers;
    assert!(incoming.iter().any(|l| l.source == shared));
    assert_eq!(s.binder.level_of(expired).0, Level::OnAir);
    assert_eq!(s.binder.level_of(late).0, Level::Placed);
    assert_eq!(s.binder.level_of(shared).0, Level::Shown);
    let heard: BTreeSet<_> = report.mixers[&s.wide]
        .inputs
        .iter()
        .map(|input| input.source)
        .collect();
    assert!(heard.contains(&expired));
    assert!(heard.contains(&shared));
    assert!(!heard.contains(&late));
    let comp = s.binder.compositor(s.wide, Role::Program).unwrap();
    assert!(links_into(&s, comp).contains(&s.binder.source_node(expired).unwrap()));
    assert!(!links_into(&s, comp).contains(&s.binder.source_node(late).unwrap()));

    // Normal visibility and geometry edits remain live despite the frozen cues.
    s.col.set_item_visible(late_item, true).unwrap();
    s.col
        .set_item_layout(expired_item, None, full().with_opacity(0.5))
        .unwrap();
    let report = s
        .binder
        .sync_transition(
            &s.col,
            &mut s.graph,
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeMap::new(),
            &outgoing,
        )
        .unwrap();
    let program = &report.compositors[&(s.wide, Role::Program)].layers;
    assert!(program.iter().any(|l| l.source == late));
    assert_eq!(
        program
            .iter()
            .find(|l| l.source == expired)
            .unwrap()
            .opacity,
        0.5
    );
    assert!(
        !report.compositors[&(s.wide, Role::Incoming)]
            .layers
            .iter()
            .any(|l| l.source == shared)
    );
    assert!(
        !report.mixers[&s.wide]
            .inputs
            .iter()
            .any(|input| input.source == shared)
    );
}
