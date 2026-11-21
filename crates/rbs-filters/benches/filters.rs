//! Filter node cost at 1920x1080 (spec `video-filters.md`, "Performance requirements").
//!
//! `record_submit`: one render tick of feed -> filter -> probe, CPU time to record and
//! submit. `frame`: the same tick and a wait for the GPU. The values alternate between
//! two sets every tick so the filter renders each time; `unchanged_input` is a tick on
//! the same input and values (the skip path: one `touch` per target). Skips without
//! an adapter.
//!
//! `pointer/idle` and `pointer/drawing`: a type with `Needs::POINTER` (spec "Host
//! service: pointer") on an unchanged input, with no live effect (pass-through) and
//! with one live effect every tick (effect buffer write and one pass).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::hint::black_box;
use std::sync::{Arc, Mutex};

use criterion::{Criterion, criterion_group, criterion_main};
use rbs_capture::{MappingCell, PictureMapping};
use rbs_core::capability::{CapabilitySet, CapabilityValue};
use rbs_core::media::Size;
use rbs_core::time::Timestamp;
use rbs_filters::conformance::{Picture, Rig};
use rbs_filters::{
    Effect, Effects, Field, FieldKind, FilterType, Needs, Pass, PassSource, PointerFeed,
    PointerView, Scale, Uniforms, Value, Values, catalogue,
};
use rbs_gpu::{GpuContext, GpuError, GpuRequest};
use rbs_graph::node::NodeTypeId;
use rbs_input::{
    Button, DesktopPoint, EventMask, InputBackend, InputError, InputEvent, InputMonitor, InputSink,
    RunningInput, TimedEvent, caps,
};

/// One effect per click, forever; draws a ring at it.
struct Ring;

static FIELDS: [Field; 1] = [Field::number("width", "Width", 1.0, 16.0, 4.0, 1.0, "px")];
static PASSES: [Pass; 1] = [Pass {
    shader: r#"
@fragment
fn fs_main(in: FilterIn) -> @location(0) vec4<f32> {
    var c = sample_linear(in.uv);
    let px = in.position.xy;
    for (var i = 0u; i < effect_count(); i++) {
        let e = effect(i, 0u);
        let d = abs(distance(px, e.xy) - 40.0);
        let a = 1.0 - smoothstep(0.0, u[0].x, d);
        c = mix(c, vec4<f32>(1.0, 0.8, 0.0, 1.0), a);
    }
    return c;
}
"#,
    target: Scale::Full,
    source: PassSource::Input,
}];

impl FilterType for Ring {
    fn type_id(&self) -> NodeTypeId {
        NodeTypeId::new_static("org.rbs.filter.bench-ring")
    }
    fn name(&self) -> &'static str {
        "Bench ring"
    }
    fn fields(&self) -> &'static [Field] {
        &FIELDS
    }
    fn passes(&self) -> &'static [Pass] {
        &PASSES
    }
    fn uniforms(&self, values: &Values, _: [u32; 2], out: &mut [Uniforms]) {
        out[0].0[0][0] = values.number(0);
    }
    fn needs(&self) -> Needs {
        Needs::POINTER
    }
    fn effects(&self, _: &Values, view: &PointerView<'_>, out: &mut Effects) {
        for click in view.clicks {
            out.push(Effect([[click.at[0], click.at[1], 0.0, 0.0], [0.0; 4]]));
        }
    }
}

#[derive(Clone, Default)]
struct Scripted(Arc<Mutex<Option<InputSink>>>);
struct Running;
impl RunningInput for Running {
    fn set_mask(&mut self, _: EventMask) {}
    fn stop(self: Box<Self>) {}
}
impl InputBackend for Scripted {
    fn name(&self) -> &'static str {
        "bench"
    }
    fn capabilities(&self) -> CapabilitySet {
        CapabilitySet::new()
            .with(caps::POINTER, CapabilityValue::Flag(true))
            .with(caps::BUTTONS, CapabilityValue::Flag(true))
    }
    fn start(&self, _: EventMask, sink: InputSink) -> Result<Box<dyn RunningInput>, InputError> {
        *self.0.lock().unwrap() = Some(sink);
        Ok(Box::new(Running))
    }
}

fn pointer(c: &mut Criterion, ctx: &Arc<GpuContext>, (w, h): (u32, u32)) {
    let script = Scripted::default();
    let monitor = InputMonitor::new(Box::new(script.clone()));
    let cell = MappingCell::new();
    cell.publish(Some(PictureMapping::new(
        Timestamp::ZERO,
        [0.0, 0.0],
        [1.0, 1.0],
        Size::new(w, h),
    )));
    let ty: Arc<dyn FilterType> = Arc::new(Ring);
    let values = Values::defaults(ty.as_ref());
    let feed = PointerFeed::new(monitor, cell);
    let mut rig = Rig::with_pointer(Arc::clone(ctx), ty, values, Some([w, h]), feed).unwrap();
    rig.set_input(Picture::solid(w, h, [0.2, 0.6, 0.3, 1.0]));
    for _ in 0..3 {
        rig.tick();
    }
    ctx.wait_idle().unwrap();
    c.bench_function("pointer/idle", |b| {
        b.iter(|| {
            rig.tick();
            black_box(());
        });
    });
    let sink = script.0.lock().unwrap().take().unwrap();
    sink.event(TimedEvent {
        time: Timestamp::from_nanos(1),
        event: InputEvent::ButtonDown {
            button: Button::Left,
            at: DesktopPoint::new(f64::from(w) / 2.0, f64::from(h) / 2.0),
        },
    });
    c.bench_function("pointer/drawing", |b| {
        b.iter(|| {
            rig.tick();
            black_box(());
        });
        ctx.wait_idle().unwrap();
    });
}

fn bench(c: &mut Criterion) {
    let ctx = match GpuContext::new(&GpuRequest {
        allow_software: true,
        ..GpuRequest::default()
    }) {
        Ok(ctx) => Arc::new(ctx),
        Err(GpuError::NoAdapter) => {
            eprintln!("SKIPPED: no GPU adapter");
            return;
        }
        Err(e) => panic!("{e}"),
    };
    let (w, h) = (1920, 1080);
    pointer(c, &ctx, (w, h));
    for ty in catalogue() {
        let name = ty
            .type_id()
            .as_str()
            .trim_start_matches("org.rbs.filter.")
            .to_owned();
        let values = Values::defaults(*ty);
        let other = ty
            .fields()
            .iter()
            .find_map(|f| match f.kind {
                FieldKind::Number { min, max, .. } => Some((f.key, (min + max) / 2.0)),
                _ => None,
            })
            .map(|(k, v)| values.with(*ty, [(k, Value::Number(v))]).unwrap())
            .unwrap();
        let mut flip = false;
        let mut next = move || {
            flip = !flip;
            if flip { other } else { values }
        };
        let mut rig = Rig::new(Arc::clone(&ctx), Arc::new(*ty), values, Some([w, h])).unwrap();
        rig.set_input(Picture::solid(w, h, [0.2, 0.6, 0.3, 1.0]));
        for _ in 0..3 {
            rig.tick();
        }
        ctx.wait_idle().unwrap();
        c.bench_function(&format!("{name}/unchanged_input"), |b| {
            b.iter(|| {
                rig.tick();
                black_box(());
            });
        });
        c.bench_function(&format!("{name}/record_submit"), |b| {
            b.iter(|| {
                rig.set_values(0, next()).unwrap();
                rig.tick();
                black_box(());
            });
            ctx.wait_idle().unwrap();
        });
        c.bench_function(&format!("{name}/frame"), |b| {
            b.iter(|| {
                rig.set_values(0, next()).unwrap();
                rig.tick();
                ctx.wait_idle().unwrap();
            });
        });
    }
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(20);
    targets = bench
}
criterion_main!(benches);
