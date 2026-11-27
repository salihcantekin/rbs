//! The filter type of background removal and the registry of person mask backends the
//! composition root installs (ADR-0179 decisions 1, 5 and 7; rule 19).

use std::sync::{Arc, OnceLock};

use rbs_core::media::Size;
use rbs_filters::{CustomNode, FilterType, Needs, Pass, PassSource, Scale, Uniforms};
use rbs_graph::node::NodeTypeId;
use rbs_params::{Field, Values};
use rbs_segment::{
    LevelId, ModelRef, PersonMaskBackend, PersonMaskSession, SegmentError, SegmentHost,
    SessionRequest,
};

use crate::node::BackgroundRemovalNode;
use crate::{COMPOSITE, NAME, QUALITY, TYPE_ID, fields};

static BACKENDS: OnceLock<Vec<Arc<dyn PersonMaskBackend>>> = OnceLock::new();

/// Installs the person mask backends of this build, in order of preference (the first
/// available one runs). Call once at start-up from a composition root; returns `false`
/// when backends were installed already.
pub fn install_backends(backends: Vec<Arc<dyn PersonMaskBackend>>) -> bool {
    BACKENDS.set(backends).is_ok()
}

/// No model files yet (they are add-ons, ADR-0179 decision 8): OS backends need none.
struct NoModels;

impl SegmentHost for NoModels {
    fn model(&self, _name: &str) -> Option<ModelRef> {
        None
    }
}

/// The first installed backend this computer can run, and the level to open it at: the
/// requested one, else the nearest it offers.
fn pick(level: LevelId) -> Option<(Arc<dyn PersonMaskBackend>, LevelId)> {
    let order = [LevelId::Low, LevelId::Medium, LevelId::High];
    let rank = |l: LevelId| order.iter().position(|o| *o == l).unwrap_or(1);
    BACKENDS.get()?.iter().find_map(|backend| {
        let info = backend.probe(&NoModels);
        if !info.available {
            return None;
        }
        let nearest = info
            .levels
            .iter()
            .map(|l| l.id)
            .min_by_key(|l| rank(*l).abs_diff(rank(level)))?;
        Some((Arc::clone(backend), nearest))
    })
}

/// Whether some installed backend can run here: what makes Add filter offer the type
/// (P3, ADR-0179 decision 7). Cheap: a probe allocates nothing and starts no thread.
pub fn available() -> bool {
    pick(LevelId::Medium).is_some()
}

/// The small picture's size for a source picture of `size` at `level`: the same aspect,
/// the longer side 256, 384 or 512 pixels.
fn small_size(size: Option<[u32; 2]>, level: LevelId) -> [u32; 2] {
    let long = match level {
        LevelId::Low => 256.0,
        LevelId::High => 512.0,
        _ => 384.0,
    };
    let [w, h] = size.filter(|s| s[0] > 0 && s[1] > 0).unwrap_or([16, 9]);
    let scale = long / w.max(h) as f32;
    [
        ((w as f32 * scale).round() as u32).max(1),
        ((h as f32 * scale).round() as u32).max(1),
    ]
}

/// The level a quality setting stands for.
fn level_of(index: u8) -> LevelId {
    match index {
        0 => LevelId::Low,
        2 => LevelId::High,
        _ => LevelId::Medium,
    }
}

/// `org.rbs.filter.background-removal`: the type listed in the catalogue
/// ([`rbs_filters::install_extra`]). Its own node, not the generic pass node, runs it.
#[derive(Debug, Clone, Copy, Default)]
pub struct BackgroundRemoval;

/// The one instance the composition root installs.
pub static BACKGROUND_REMOVAL: BackgroundRemoval = BackgroundRemoval;

// The generic node would draw this pass; the studio never uses it for this type
// (`custom_node`), but a type has at least one pass and the conformance suite compiles it.
const PASSES: [Pass; 1] = [Pass {
    shader: COMPOSITE,
    target: Scale::Full,
    source: PassSource::Input,
}];

impl FilterType for BackgroundRemoval {
    fn type_id(&self) -> NodeTypeId {
        NodeTypeId::new_static(TYPE_ID)
    }
    fn name(&self) -> &'static str {
        NAME
    }
    fn fields(&self) -> &'static [Field] {
        fields()
    }
    fn passes(&self) -> &'static [Pass] {
        &PASSES
    }
    fn uniforms(&self, _values: &Values, _input: [u32; 2], out: &mut [Uniforms]) {
        for u in out {
            *u = Uniforms::default();
        }
    }
    fn needs(&self) -> Needs {
        Needs::PERSON_MASK
    }
    fn custom_node(&self, values: Values, size: Option<[u32; 2]>) -> Option<CustomNode> {
        let level = level_of(values.choice(QUALITY));
        let picked = pick(level);
        let level = picked.as_ref().map_or(level, |(_, l)| *l);
        let small = small_size(size, level);
        let factory: crate::SessionFactory = Box::new(
            move || -> Result<Box<dyn PersonMaskSession>, SegmentError> {
                let (backend, level) = picked.clone().ok_or(SegmentError::Unavailable)?;
                backend.open(SessionRequest::new(level, Size::new(small[0], small[1])))
            },
        );
        let (node, values, _) = BackgroundRemovalNode::new(factory, small, values);
        let turned_on = node.turned_on();
        let stats = Arc::new(rbs_filters::FilterStats::default());
        Some(CustomNode::new(Box::new(node), values, stats).with_turned_on(turned_on))
    }
}
