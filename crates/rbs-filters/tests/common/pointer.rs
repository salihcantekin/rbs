//! A test pointer filter type and a scripted input backend (spec `video-filters.md`,
//! "Host service: pointer"), shared by the pointer tests.

use std::sync::{Arc, Mutex, PoisonError};

use rbs_capture::{Cover, PictureMapping};
use rbs_core::capability::{CapabilitySet, CapabilityValue};
use rbs_core::media::Size;
use rbs_core::time::Timestamp;
use rbs_filters::{
    Effect, Effects, Field, FilterType, Needs, Pass, PassSource, PointerView, Scale, Uniforms,
    Values,
};
use rbs_graph::node::NodeTypeId;
use rbs_input::{
    Button, DesktopPoint, EventMask, InputBackend, InputError, InputEvent, InputSink,
    PointerSample, RunningInput, TimedEvent, caps,
};

/// How long a test ring lives.
pub const LIFE_NS: u64 = 100_000_000;

/// What the test type saw on its last `effects` call.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Seen {
    pub clicks: usize,
    pub first: Option<[f32; 2]>,
    pub pointer: Option<[f32; 2]>,
}

/// Paints the whole picture red while any effect lives; one effect per click younger
/// than [`LIFE_NS`]. Each test has its own, so parallel tests do not share `seen`.
#[derive(Clone)]
pub struct RingType {
    seen: Arc<Mutex<Seen>>,
    life_ns: u64,
}

impl Default for RingType {
    fn default() -> Self {
        Self::living(LIFE_NS)
    }
}

impl RingType {
    /// A type whose rings live `life_ns`.
    pub fn living(life_ns: u64) -> Self {
        Self {
            seen: Arc::default(),
            life_ns,
        }
    }
    pub fn seen(&self) -> Seen {
        *self.seen.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

static FIELDS: [Field; 1] = [Field::number(
    "strength", "Strength", 0.0, 1.0, 1.0, 0.1, "",
)];
static PASSES: [Pass; 1] = [Pass {
    shader: r#"
@fragment
fn fs_main(in: FilterIn) -> @location(0) vec4<f32> {
    if (effect_count() > 0u && effect(0u, 0u).w > 0.5) {
        return vec4<f32>(1.0, 0.0, 0.0, 1.0);
    }
    return sample_linear(in.uv);
}
"#,
    target: Scale::Full,
    source: PassSource::Input,
}];

impl FilterType for RingType {
    fn type_id(&self) -> NodeTypeId {
        NodeTypeId::new_static("org.rbs.filter.test-ring")
    }
    fn name(&self) -> &'static str {
        "Test ring"
    }
    fn fields(&self) -> &'static [Field] {
        &FIELDS
    }
    fn passes(&self) -> &'static [Pass] {
        &PASSES
    }
    fn uniforms(&self, _: &Values, _: [u32; 2], _: &mut [Uniforms]) {}
    fn needs(&self) -> Needs {
        Needs::POINTER
    }
    fn effects(&self, _: &Values, view: &PointerView<'_>, out: &mut Effects) {
        *self.seen.lock().unwrap_or_else(PoisonError::into_inner) = Seen {
            clicks: view.clicks.len(),
            first: view.clicks.first().map(|c| c.at),
            pointer: view.pointer,
        };
        for click in view.clicks {
            let age = view.now.saturating_since(click.time);
            if click.time <= view.now && age.as_nanos() < u128::from(self.life_ns) {
                out.push(Effect([[click.at[0], click.at[1], 0.0, 1.0], [0.0; 4]]));
            }
        }
    }
}

/// An input backend the test drives: it keeps the sink and fails `start` on request.
#[derive(Clone, Default)]
pub struct Scripted(pub Arc<Mutex<(Option<InputSink>, bool)>>);

struct Running(Scripted);

impl RunningInput for Running {
    fn set_mask(&mut self, _: EventMask) {}
    fn stop(self: Box<Self>) {
        self.0.0.lock().unwrap_or_else(PoisonError::into_inner).0 = None;
    }
}

impl InputBackend for Scripted {
    fn name(&self) -> &'static str {
        "scripted"
    }
    fn capabilities(&self) -> CapabilitySet {
        CapabilitySet::new()
            .with(caps::POINTER, CapabilityValue::Flag(true))
            .with(caps::BUTTONS, CapabilityValue::Flag(true))
    }
    fn start(&self, _: EventMask, sink: InputSink) -> Result<Box<dyn RunningInput>, InputError> {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if state.1 {
            return Err(InputError::Backend { code: 1 });
        }
        state.0 = Some(sink);
        Ok(Box::new(Running(self.clone())))
    }
}

impl Scripted {
    pub fn click(&self, ns: u64, x: f64, y: f64) {
        let state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let sink = state.0.as_ref().expect("the node subscribed");
        sink.event(TimedEvent {
            time: Timestamp::from_nanos(ns),
            event: InputEvent::ButtonDown {
                button: Button::Left,
                at: DesktopPoint::new(x, y),
            },
        });
    }
    pub fn point(&self, ns: u64, x: f64, y: f64) {
        let state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let sink = state.0.as_ref().expect("the node subscribed");
        sink.pointer(PointerSample {
            time: Timestamp::from_nanos(ns),
            at: DesktopPoint::new(x, y),
        });
    }
}

/// The picture sits at desktop (100, 50), one picture pixel per desktop unit.
pub fn mapping(cover: Cover) -> PictureMapping {
    PictureMapping::new(Timestamp::ZERO, [100.0, 50.0], [1.0, 1.0], Size::new(8, 8))
        .with_cover(cover)
}
