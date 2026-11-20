//! Field edits applied in place (audit F-09; scene model spec, "Undo"): volume, mute,
//! monitor, sync offset and master. A refused one changes nothing, and undoing a run
//! of them restores every earlier state exactly.

#![allow(clippy::unwrap_used)] // Tests fail by panicking.

use rbs_core::media::Size;
use rbs_core::timing::FrameRate;
use rbs_graph::node::NodeTypeId;
use rbs_scene::model::{
    CanvasId, MAX_SOURCE_VOLUME, MAX_SYNC_OFFSET_MS, Settings, SourceId, SourceMonitor,
};
use rbs_scene::{SceneCollection, SceneError, Undo};

struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn studio() -> (SceneCollection, CanvasId, Vec<SourceId>) {
    let mut col = SceneCollection::new();
    let (canvas, _) = col
        .add_canvas("Main", Size::new(1920, 1080), FrameRate::FPS_60)
        .unwrap();
    let sources = (0..4)
        .map(|i| {
            let ty = NodeTypeId::new("org.rbs.source.camera");
            col.add_source(&format!("S{i}"), ty, Settings::new())
                .unwrap()
                .0
        })
        .collect();
    (col, canvas, sources)
}

/// One random field edit; about one in five is out of range or names an unknown id.
fn field_edit(
    col: &mut SceneCollection,
    canvas: CanvasId,
    sources: &[SourceId],
    rng: &mut Lcg,
) -> Result<Undo, SceneError> {
    let bad = rng.below(5) == 0;
    let source = if bad && rng.below(2) == 0 {
        SourceId::from_raw(999)
    } else {
        sources[rng.below(sources.len() as u64) as usize]
    };
    let level = if bad {
        MAX_SOURCE_VOLUME + 1.0
    } else {
        rng.below(11) as f32 / 10.0
    };
    let flag = rng.below(2) == 0;
    match rng.below(7) {
        0 => col.set_source_volume(source, level),
        1 => col.set_source_muted(source, flag),
        2 => col.set_source_mixer_hidden(source, flag),
        3 => {
            let monitor = [SourceMonitor::Off, SourceMonitor::Only, SourceMonitor::Also]
                [rng.below(3) as usize];
            col.set_source_monitor(source, monitor)
        }
        4 => {
            let offset = if bad {
                MAX_SYNC_OFFSET_MS + 1
            } else {
                rng.below(100) as i32
            };
            col.set_source_sync_offset(source, offset)
        }
        5 => col.set_canvas_master_volume(canvas, level),
        _ => col.set_canvas_master_muted(canvas, flag),
    }
}

#[test]
fn refused_field_edits_change_nothing_and_undo_restores_every_state() {
    for seed in 0..40_u64 {
        let mut rng = Lcg(seed);
        let (mut col, canvas, sources) = studio();
        let mut states = vec![col.clone()];
        let mut undos = Vec::new();
        let mut refused = 0;
        for _ in 0..200 {
            let (before, generation) = (col.clone(), col.generation());
            match field_edit(&mut col, canvas, &sources, &mut rng) {
                Ok(undo) => {
                    assert_eq!(col.generation(), generation + 1, "seed {seed}");
                    undos.push(undo);
                    states.push(col.clone());
                }
                Err(_) => {
                    refused += 1;
                    assert_eq!(col, before, "seed {seed}: a refused edit changed state");
                    assert_eq!(col.generation(), generation, "seed {seed}");
                }
            }
        }
        assert!(
            refused > 0 && undos.len() > 100,
            "seed {seed}: {refused} refused"
        );
        let last = col.clone();
        let mut redos = Vec::new();
        while let Some(undo) = undos.pop() {
            states.pop();
            redos.push(col.undo(undo).unwrap());
            assert_eq!(&col, states.last().unwrap(), "seed {seed}: undo mismatch");
        }
        while let Some(redo) = redos.pop() {
            col.undo(redo).unwrap();
        }
        assert_eq!(col, last, "seed {seed}: redo mismatch");
    }
}

#[test]
fn a_stale_field_record_is_refused_and_changes_nothing() {
    let (mut col, canvas, sources) = studio();
    let quiet = col.set_source_volume(sources[0], 0.2).unwrap();
    let master = col.set_canvas_master_volume(canvas, 0.4).unwrap();
    col.set_source_volume(sources[0], 0.6).unwrap();
    col.set_canvas_master_muted(canvas, true).unwrap();
    let before = col.clone();
    assert_eq!(col.undo(quiet), Err(SceneError::UndoConflict));
    assert_eq!(col.undo(master), Err(SceneError::UndoConflict));
    assert_eq!(col, before);
}
