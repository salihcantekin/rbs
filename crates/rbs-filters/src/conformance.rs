//! The conformance suite every [`FilterType`] must pass (spec `video-filters.md`,
//! "Conformance tests"), and a small rig that runs one filter node on the graph
//! runtime for tests and benchmarks.
//!
//! Not for real-time use: the rig's feed and probe nodes allocate and lock.

#![allow(clippy::missing_panics_doc)]

use std::sync::{Arc, Mutex, PoisonError};

use rbs_core::lifecycle::Lifecycle;
use rbs_core::media::{
    ColorSpace, FramePayload, PixelFormat, Size, TextureId, VideoFormat, VideoFrame,
};
use rbs_core::time::Timestamp;
use rbs_core::timing::{Cadence, FrameRate};
use rbs_gpu::node::{GpuRuntime, RenderLocal};
use rbs_gpu::{GpuContext, TextureDesc, readback};
use rbs_graph::description::{GraphDescription, NodeId};
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::param::ParamSender;
use rbs_graph::runtime::{
    Controller, Executor, Node, NodeError, PrepareCtx, ProcessCtx, Tick, channel,
};

use crate::node::{FilterNode, FilterStats, PRELUDE};
use crate::types::{
    FieldKind, FilterType, MAX_FIELDS, MAX_PASSES, ParamError, PassSource, Scale, Uniforms, Value,
    Values,
};

/// Checks a type's schema, values and uniforms without a GPU. Returns every problem.
pub fn check_type(ty: &dyn FilterType) -> Vec<String> {
    let mut problems = Vec::new();
    let id = ty.type_id();
    if !id.is_well_formed() {
        problems.push(format!("type id {id} is not well-formed"));
    }
    let fields = ty.fields();
    if fields.len() > MAX_FIELDS {
        problems.push(format!("{} fields, at most {MAX_FIELDS}", fields.len()));
    }
    if let Err(e) = rbs_params::check_fields(fields, MAX_FIELDS) {
        problems.push(e);
    }
    for (i, f) in fields.iter().enumerate() {
        if f.key.is_empty()
            || !f
                .key
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        {
            problems.push(format!("key {:?} is not lowercase ASCII with _", f.key));
        }
        if fields[..i].iter().any(|g| g.key == f.key) {
            problems.push(format!("key {:?} is used twice", f.key));
        }
        if f.label.is_empty() {
            problems.push(format!("{} has no label", f.key));
        }
        if let FieldKind::Number {
            min,
            max,
            default,
            step,
            ..
        } = f.kind
            && !(min.is_finite()
                && max.is_finite()
                && min < max
                && default.is_finite()
                && (min..=max).contains(&default)
                && step.is_finite()
                && step > 0.0)
        {
            problems.push(format!("{}: range, default or step is not usable", f.key));
        }
    }
    let passes = ty.passes();
    if passes.is_empty() || passes.len() > MAX_PASSES {
        problems.push(format!("{} passes, 1 to {MAX_PASSES}", passes.len()));
    }
    if passes
        .first()
        .is_some_and(|p| p.source != PassSource::Input)
    {
        problems.push("the first pass must sample the input".to_owned());
    }
    if passes.last().is_some_and(|p| p.target != Scale::Full) {
        problems.push("the last pass must target the full size".to_owned());
    }

    for (i, pass) in passes.iter().enumerate() {
        if let Err(e) = compile(pass.shader) {
            problems.push(format!("pass {i} does not compile: {e}"));
        }
    }

    // Values: refusals and defaults.
    let defaults = Values::defaults(ty);
    if Values::from_pairs(ty, [("no_such_key", Value::Number(0.0))])
        != Err(ParamError::UnknownKey("no_such_key".to_owned()))
    {
        problems.push("an unknown key is not refused".to_owned());
    }
    for (i, f) in fields.iter().enumerate() {
        if defaults.get(i) != Some(f.kind.default_value()) {
            problems.push(format!("{}: defaults do not hold the default", f.key));
        }
        match f.kind {
            FieldKind::Number { min, max, .. } => {
                for bad in [f32::NAN, f32::INFINITY, min - 1.0, max + 1.0] {
                    if Values::from_pairs(ty, [(f.key, Value::Number(bad))]).is_ok() {
                        problems.push(format!("{}: {bad} is not refused", f.key));
                    }
                }
                if Values::from_pairs(ty, [(f.key, Value::Color([0; 4]))])
                    != Err(ParamError::WrongKind(f.key))
                {
                    problems.push(format!("{}: a colour is not refused", f.key));
                }
                for good in [min, max] {
                    match Values::from_pairs(ty, [(f.key, Value::Number(good))]) {
                        Ok(v) => check_uniforms(ty, &v, &mut problems),
                        Err(e) => problems.push(format!("{}: {good} refused: {e}", f.key)),
                    }
                }
            }
            FieldKind::Color { .. }
                if Values::from_pairs(ty, [(f.key, Value::Number(0.0))])
                    != Err(ParamError::WrongKind(f.key)) =>
            {
                problems.push(format!("{}: a number is not refused", f.key));
            }
            // Other kinds are checked by the shared schema conformance
            // (`rbs_params::check_fields`) and not used by video filters yet.
            _ => {}
        }
    }
    check_uniforms(ty, &defaults, &mut problems);
    let pairs = defaults.to_pairs(ty);
    if Values::from_pairs(ty, pairs.iter().map(|(k, v)| (*k, *v))) != Ok(defaults) {
        problems.push("to_pairs does not round-trip".to_owned());
    }
    problems
}

/// Parses and validates a pass with the prelude, as the device would, without a GPU.
pub fn compile(shader: &str) -> Result<(), String> {
    use wgpu::naga;
    let source = format!("{PRELUDE}\n{shader}");
    let module = naga::front::wgsl::parse_str(&source).map_err(|e| e.emit_to_string(&source))?;
    naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::empty(),
    )
    .validate(&module)
    .map_err(|e| format!("{e:?}"))?;
    Ok(())
}

fn check_uniforms(ty: &dyn FilterType, values: &Values, problems: &mut Vec<String>) {
    let mut out = [Uniforms::default(); MAX_PASSES];
    let n = ty.passes().len().min(MAX_PASSES);
    for size in [[1, 1], [1920, 1080], [7, 3]] {
        ty.uniforms(values, size, &mut out[..n]);
        if out[..n]
            .iter()
            .any(|u| u.0.iter().flatten().any(|x| !x.is_finite()))
        {
            problems.push(format!("uniforms of {values:?} at {size:?} are not finite"));
        }
    }
}

/// Encodes an `f32` as IEEE half (round towards zero; enough for test pictures).
pub fn f32_to_f16(v: f32) -> u16 {
    let bits = v.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exp = ((bits >> 23) & 0xff) as i32 - 127 + 15;
    let mant = ((bits >> 13) & 0x3ff) as u16;
    if v == 0.0 || exp <= 0 {
        sign
    } else if exp >= 31 {
        sign | 0x7c00
    } else {
        sign | ((exp as u16) << 10) | mant
    }
}

/// A test picture: `Rgba16Float`, linear light, premultiplied.
#[derive(Debug, Clone, PartialEq)]
pub struct Picture {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Row-major texels.
    pub pixels: Vec<[f32; 4]>,
    /// Uploaded as 8-bit sRGB-encoded (`Rgba8Unorm`, tagged sRGB) instead of
    /// `Rgba16Float` linear; `pixels` then hold the encoded values.
    pub srgb8: bool,
}

impl Picture {
    /// A `w x h` picture of one colour.
    pub fn solid(width: u32, height: u32, rgba: [f32; 4]) -> Self {
        Self {
            width,
            height,
            pixels: vec![rgba; (width * height) as usize],
            srgb8: false,
        }
    }

    /// This picture uploaded as 8-bit sRGB-encoded values.
    pub fn encoded(mut self) -> Self {
        self.srgb8 = true;
        self
    }

    /// The texel at `(x, y)`.
    pub fn at(&self, x: u32, y: u32) -> [f32; 4] {
        self.pixels[(y * self.width + x) as usize]
    }

    fn bytes(&self) -> Vec<u8> {
        if self.srgb8 {
            return self
                .pixels
                .iter()
                .flat_map(|p| p.map(|c| (c.clamp(0.0, 1.0) * 255.0).round() as u8))
                .collect();
        }
        self.pixels
            .iter()
            .flat_map(|p| p.iter().flat_map(|c| f32_to_f16(*c).to_le_bytes()))
            .collect()
    }
}

type Shared<T> = Arc<Mutex<T>>;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Publishes the latest picture it was given, uploaded to a pool texture.
struct Feed {
    desc: NodeDescriptor,
    next: Shared<Option<Picture>>,
    /// Texture, size, sRGB flag and upload time of the picture published.
    current: Option<(TextureId, u32, u32, bool, Timestamp)>,
    uploaded: Shared<Timestamp>,
}

impl Node<GpuRuntime> for Feed {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.desc
    }
    fn prepare(&mut self, _: &mut PrepareCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        Ok(())
    }
    fn process(&mut self, ctx: &mut ProcessCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        let time = ctx.tick().time;
        let gpu = ctx.shared();
        let (_, outputs, local) = ctx.parts();
        if let Some(p) = lock(&self.next).take() {
            let texel = |srgb8| {
                if srgb8 {
                    wgpu::TextureFormat::Rgba8Unorm
                } else {
                    wgpu::TextureFormat::Rgba16Float
                }
            };
            let desc = TextureDesc::source(p.width, p.height, texel(p.srgb8));
            if let Some((old, w, h, srgb8, _)) = self.current.take() {
                let _ = local.pool.release(old);
                if TextureDesc::source(w, h, texel(srgb8)) != desc {
                    // As a capture source does when its size changes: the old frame
                    // is dropped once nobody reads it.
                    local.pool.forget_texture(old);
                }
            }
            let id = local
                .pool
                .acquire(gpu, &desc)
                .map_err(|_| NodeError::Backend("feed"))?;
            readback::upload(gpu, &local.pool, id, &p.bytes())
                .map_err(|_| NodeError::Backend("feed"))?;
            self.current = Some((id, p.width, p.height, p.srgb8, time));
            *lock(&self.uploaded) = time;
        }
        // Like a source, the same frame (same time) until a new picture arrives.
        let Some((id, w, h, srgb8, time)) = self.current else {
            return Ok(());
        };
        let format = if srgb8 {
            VideoFormat::new(Size::new(w, h), PixelFormat::Rgba8, ColorSpace::SRGB)
        } else {
            VideoFormat::new(
                Size::new(w, h),
                PixelFormat::Rgba16Float,
                ColorSpace::LINEAR_BT709,
            )
        };
        let frame = local
            .pool
            .frame(id, format, time)
            .map_err(|_| NodeError::Backend("feed"))?;
        drop(outputs.set_video(0, Some(frame))?);
        Ok(())
    }
    fn release(&mut self, _: &mut PrepareCtx<'_, GpuRuntime>) {}
}

/// Acquires and releases a canvas texture every tick, as the compositor does.
struct CanvasUser {
    desc: NodeDescriptor,
    size: [u32; 2],
}

impl Node<GpuRuntime> for CanvasUser {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.desc
    }
    fn prepare(&mut self, _: &mut PrepareCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        Ok(())
    }
    fn process(&mut self, ctx: &mut ProcessCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        let gpu = ctx.shared();
        let (_, _, local) = ctx.parts();
        let id = local
            .pool
            .acquire(gpu, &TextureDesc::canvas(self.size[0], self.size[1]))
            .map_err(|_| NodeError::Backend("canvas"))?;
        local
            .pool
            .release(id)
            .map_err(|_| NodeError::Backend("canvas"))
    }
    fn release(&mut self, _: &mut PrepareCtx<'_, GpuRuntime>) {}
}

/// Keeps the last frame it received (or that it received none).
struct Probe {
    desc: NodeDescriptor,
    last: Shared<Option<VideoFrame>>,
}

impl Node<GpuRuntime> for Probe {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.desc
    }
    fn prepare(&mut self, _: &mut PrepareCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        Ok(())
    }
    fn process(&mut self, ctx: &mut ProcessCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        *lock(&self.last) = ctx.inputs().video(0, 0).and_then(VideoFrame::share);
        Ok(())
    }
    fn release(&mut self, _: &mut PrepareCtx<'_, GpuRuntime>) {}
}

/// Feed -> filter -> probe on one render executor, driven tick by tick on the calling
/// thread.
pub struct Rig {
    graph: GraphDescription,
    ctl: Controller<GpuRuntime>,
    exec: Executor<GpuRuntime>,
    ctx: Arc<GpuContext>,
    tick: u64,
    next: Shared<Option<Picture>>,
    uploaded: Shared<Timestamp>,
    last: Shared<Option<VideoFrame>>,
    /// The (first) filter node's id.
    pub filter: NodeId,
    /// The (first) filter's value sender.
    pub values: ParamSender<Values>,
    /// The (first) filter's statistics.
    pub stats: Arc<FilterStats>,
    /// Every filter of the chain, in order: node, statistics.
    pub chain: Vec<(NodeId, Arc<FilterStats>)>,
    senders: Vec<ParamSender<Values>>,
}

/// A rig ends with the GPU idle. A test that ends with work in flight (no readback
/// after its last tick) otherwise lets the process exit while a software driver's
/// threads still execute it, which crashed a test binary on exit in CI (SIGSEGV after
/// "test result: ok").
impl Drop for Rig {
    fn drop(&mut self) {
        let _ = self.ctx.wait_idle();
    }
}

impl Rig {
    /// A rig running `ty` with `values`, warmed for `size` when given.
    pub fn new(
        ctx: Arc<GpuContext>,
        ty: Arc<dyn FilterType>,
        values: Values,
        size: Option<[u32; 2]>,
    ) -> Result<Self, String> {
        Self::chain(ctx, vec![(ty, values)], size)
    }

    /// A rig running a chain of filters in order (at least one), each warmed for
    /// `size` when given.
    pub fn chain(
        ctx: Arc<GpuContext>,
        filters: Vec<(Arc<dyn FilterType>, Values)>,
        size: Option<[u32; 2]>,
    ) -> Result<Self, String> {
        Self::build(ctx, filters, size, None)
    }

    /// Like [`Rig::chain`], with a node that acquires and releases a canvas texture
    /// of `canvas` every tick, as the compositor does: a second user of the filters'
    /// texture description when the sizes match.
    pub fn chain_with_canvas(
        ctx: Arc<GpuContext>,
        filters: Vec<(Arc<dyn FilterType>, Values)>,
        size: Option<[u32; 2]>,
        canvas: [u32; 2],
    ) -> Result<Self, String> {
        Self::build(ctx, filters, size, Some(canvas))
    }

    fn build(
        ctx: Arc<GpuContext>,
        filters: Vec<(Arc<dyn FilterType>, Values)>,
        size: Option<[u32; 2]>,
        canvas: Option<[u32; 2]>,
    ) -> Result<Self, String> {
        let (mut ctl, exec) = channel::<GpuRuntime>(Arc::clone(&ctx), RenderLocal::default());
        let mut graph = GraphDescription::new();
        let next = Arc::new(Mutex::new(None));
        let uploaded = Arc::new(Mutex::new(Timestamp::ZERO));
        let last = Arc::new(Mutex::new(None));
        let feed = Feed {
            desc: NodeDescriptor::new(
                NodeTypeId::new_static("org.rbs.test.feed"),
                NodeKind::Source,
                vec![],
                vec![Pin::one("out", LinkKind::Video)],
            ),
            next: Arc::clone(&next),
            current: None,
            uploaded: Arc::clone(&uploaded),
        };
        let probe = Probe {
            desc: NodeDescriptor::new(
                NodeTypeId::new_static("org.rbs.test.probe"),
                NodeKind::Output,
                vec![Pin::one("in", LinkKind::Video)],
                vec![],
            ),
            last: Arc::clone(&last),
        };
        let e = |e: &dyn std::fmt::Display| e.to_string();
        let mut add = |node: Box<dyn Node<GpuRuntime>>| -> Result<NodeId, String> {
            let id = graph
                .add_node(Arc::new(node.descriptor().clone()))
                .map_err(|x| e(&x))?;
            graph.set_state(id, Lifecycle::Standby).map_err(|x| e(&x))?;
            graph.set_state(id, Lifecycle::Active).map_err(|x| e(&x))?;
            ctl.adopt(id, node).map_err(|x| e(&x))?;
            Ok(id)
        };
        let mut links = vec![];
        let feed_id = add(Box::new(feed))?;
        if let Some(canvas) = canvas {
            let id = add(Box::new(CanvasUser {
                desc: NodeDescriptor::new(
                    NodeTypeId::new_static("org.rbs.test.canvas"),
                    NodeKind::Output,
                    vec![Pin::one("in", LinkKind::Video)],
                    vec![],
                ),
                size: canvas,
            }))?;
            links.push((feed_id, id));
        }
        let mut prev = feed_id;
        let mut chain = Vec::new();
        let mut senders = Vec::new();
        for (ty, values) in filters {
            let (node, sender, stats) = FilterNode::new(ty, values, size);
            let id = add(Box::new(node))?;
            links.push((prev, id));
            chain.push((id, stats));
            senders.push(sender);
            prev = id;
        }
        let probe = add(Box::new(probe))?;
        links.push((prev, probe));
        for (from, to) in links {
            graph.connect(from, "out", to, "in").map_err(|x| e(&x))?;
        }
        if senders.is_empty() {
            return Err("a rig needs a filter".to_owned());
        }
        let values = senders.remove(0);
        let (filter, stats) = (chain[0].0, Arc::clone(&chain[0].1));
        let mut rig = Self {
            graph,
            ctl,
            exec,
            ctx,
            tick: 0,
            next,
            uploaded,
            last,
            filter,
            values,
            stats,
            chain,
            senders,
        };
        let report = rig.ctl.apply(&rig.graph).map_err(|x| e(&x))?;
        if !report.failed.is_empty() {
            return Err(format!("apply failed: {:?}", report.failed));
        }
        Ok(rig)
    }

    /// Textures holding GPU memory in the executor's pool.
    pub fn allocated(&mut self) -> usize {
        self.exec.local().pool.allocated()
    }

    /// Textures in the executor's pool that are handed out or still read.
    pub fn in_use(&mut self) -> usize {
        self.exec.local().pool.in_use()
    }

    /// Takes the first filter out of the schedule (Standby), as disabling it does.
    pub fn disable_filter(&mut self) -> Result<(), String> {
        self.graph
            .set_state(self.filter, Lifecycle::Standby)
            .map_err(|e| e.to_string())?;
        self.ctl.apply(&self.graph).map_err(|e| e.to_string())?;
        // The executor swaps schedules after its next tick. The probe is downstream of a
        // node that no longer runs: drop the last frame it kept, as a real consumer
        // would on its next frame.
        self.tick();
        *lock(&self.last) = None;
        Ok(())
    }

    /// The picture the feed publishes from the next tick on.
    pub fn set_input(&mut self, picture: Picture) {
        *lock(&self.next) = Some(picture);
    }

    /// Runs one tick.
    pub fn tick(&mut self) {
        let cadence = Cadence::frames(Timestamp::ZERO, FrameRate::FPS_60);
        self.exec.run_tick(Tick::of(&cadence, self.tick));
        self.tick += 1;
        let _ = self.ctl.collect();
    }

    /// Time of the picture the feed publishes: the tick it was uploaded on.
    pub fn input_time(&self) -> Timestamp {
        *lock(&self.uploaded)
    }

    /// Sends new values to filter `index` of the chain (0 is the first).
    pub fn set_values(&mut self, index: usize, values: Values) -> Result<(), String> {
        match index {
            0 => self.values.send(values),
            i => self
                .senders
                .get_mut(i - 1)
                .ok_or_else(|| format!("no filter {i}"))?
                .send(values),
        };
        Ok(())
    }

    /// Time of the tick run last.
    pub fn last_time(&self) -> Timestamp {
        let cadence = Cadence::frames(Timestamp::ZERO, FrameRate::FPS_60);
        Tick::of(&cadence, self.tick.saturating_sub(1)).time
    }

    /// Errors in a row of the filter node, and whether the executor bypassed it.
    pub fn filter_health(&self) -> Option<(u32, bool)> {
        self.exec
            .health(self.filter)
            .map(|h| (h.consecutive_errors, h.bypassed))
    }

    /// The last error summary of the filter node.
    pub fn filter_error(&self) -> Option<&'static str> {
        self.exec.health(self.filter).and_then(|h| h.last_error)
    }

    /// The frame the probe saw on the last tick: its picture, format and time; `None`
    /// when the filter published nothing.
    pub fn output(&mut self) -> Option<(Picture, VideoFormat, Timestamp)> {
        let frame = lock(&self.last).as_ref().and_then(VideoFrame::share)?;
        let FramePayload::Texture(id) = *frame.payload() else {
            return None;
        };
        let format = *frame.format();
        let px = readback::read_rgba16f(&self.ctx, &self.exec.local().pool, id).ok()?;
        let size = format.size;
        Some((
            Picture {
                width: size.width,
                height: size.height,
                pixels: px,
                srgb8: false,
            },
            format,
            frame.timestamp(),
        ))
    }
}

fn close(a: [f32; 4], b: [f32; 4], tol: f32) -> bool {
    a.iter().zip(b).all(|(x, y)| (x - y).abs() <= tol)
}

/// Runs the GPU part of the suite on `ty` with its default values. Returns every
/// problem.
pub fn check_on_gpu(ty: Arc<dyn FilterType>, ctx: Arc<GpuContext>) -> Vec<String> {
    let mut problems = Vec::new();
    let values = Values::defaults(ty.as_ref());
    let mut rig = match Rig::new(ctx, Arc::clone(&ty), values, Some([8, 4])) {
        Ok(r) => r,
        Err(e) => return vec![e],
    };
    // 4: size, format, time.
    rig.set_input(Picture::solid(8, 4, [0.25, 0.5, 0.125, 1.0]));
    rig.tick();
    rig.tick();
    match rig.output() {
        Some((p, f, t)) => {
            if (p.width, p.height) != (8, 4) {
                problems.push(format!("output is {}x{}, input 8x4", p.width, p.height));
            }
            if f.pixel_format != PixelFormat::Rgba16Float || f.color != ColorSpace::LINEAR_BT709 {
                problems.push(format!("output format {f:?}"));
            }
            if t != rig.input_time() {
                problems.push("output time is not the input's".to_owned());
            }
        }
        None => problems.push("no output for an input of the warm size".to_owned()),
    }
    // 5: transparent stays transparent.
    rig.set_input(Picture::solid(8, 4, [0.0; 4]));
    rig.tick();
    match rig.output() {
        Some((p, _, _)) => {
            if p.pixels.iter().any(|px| !close(*px, [0.0; 4], 1e-3)) {
                problems.push("a transparent input does not stay transparent".to_owned());
            }
        }
        None => problems.push("no output for a transparent input".to_owned()),
    }
    // 6: a new size is filtered at once, never the input itself.
    let before = rig.stats.resizes();
    rig.set_input(Picture::solid(7, 3, [1.0, 0.0, 0.0, 1.0]));
    rig.tick();
    match rig.output() {
        Some((p, _, _)) if (p.width, p.height) == (7, 3) => {}
        Some((p, _, _)) => problems.push(format!("after a size change: {}x{}", p.width, p.height)),
        None => problems.push("no output on a size change".to_owned()),
    }
    if rig.stats.resizes() != before + 1 || rig.stats.warm_size() != Some([7, 3]) {
        problems.push("a size change is not counted or stored".to_owned());
    }
    if rig
        .filter_health()
        .is_some_and(|(errors, bypassed)| errors > 0 || bypassed)
    {
        problems.push("a size change made the node fail".to_owned());
    }
    // 7: the same input frame and values are not filtered again; the output stays.
    let frames = rig.stats.frames();
    for _ in 0..3 {
        rig.tick();
    }
    if rig.stats.frames() != frames {
        problems.push("an unchanged input was filtered again".to_owned());
    }
    if rig.output().is_none() {
        problems.push("the output went away while the input was unchanged".to_owned());
    }
    if rig.set_values(0, values).is_err() {
        problems.push("no value channel".to_owned());
    }
    rig.tick();
    if rig.stats.frames() != frames + 1 {
        problems.push("new values did not filter the unchanged input again".to_owned());
    }
    problems
}
