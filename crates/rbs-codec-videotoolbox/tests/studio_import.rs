//! The studio can use what the decoder hands out: native NV12 IOSurface pictures of the
//! VideoToolbox decoder are imported through `rbs-gpu`'s Metal importer (as the media
//! picture node does), drawn by the compositor through its 4:2:0 path and read back,
//! with a flat red/blue clip. Also checks that a leased surface is not written while the
//! lease lives, and that slots come back after the importer hands the leases back.
//!
//! Skips with a reason when there is no GPU context or encoder;
//! `RBS_REQUIRE_VIDEOTOOLBOX=1` turns the skips into failures.

#![cfg(target_os = "macos")]
#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod decode_support;

use rbs_codec::{DecodeOutput, VideoDecoder, VideoDecoderConfig};
use rbs_codec_videotoolbox::{VideoToolboxDecoderFactory, VtH264Decoder};
use rbs_core::media::{FramePayload, MemoryKind, NativeSurface, PixelFormat, Size, VideoFrame};
use rbs_core::time::Timestamp;
use rbs_gpu::{
    CanvasDesc, Compositor, GpuContext, GpuRequest, Importer, Layer, TextureDesc, TextureId,
    TexturePool, readback,
};

const W: u32 = 128;
const H: u32 = 96;

/// BT.709 limited-range Y'CbCr of pure red and pure blue.
const RED: (u8, u8, u8) = (63, 102, 240);
const BLUE: (u8, u8, u8) = (32, 240, 118);
const RED_RGB: [f32; 3] = [1.0, 0.0, 0.0];
const BLUE_RGB: [f32; 3] = [0.0, 0.0, 1.0];
/// Encoding a flat colour moves it by a few code values.
const TOLERANCE: f32 = 0.06;

fn required() -> bool {
    std::env::var("RBS_REQUIRE_VIDEOTOOLBOX").is_ok_and(|v| v == "1")
}

fn skip(why: &str) {
    assert!(!required(), "RBS_REQUIRE_VIDEOTOOLBOX=1 but {why}");
    eprintln!("SKIPPED: {why}");
}

fn decoder(surfaces: usize) -> VtH264Decoder {
    let config = VideoDecoderConfig::h264_annex_b(Size::new(1920, 1088))
        .with_target(DecodeOutput::system().accepting(MemoryKind::Metal))
        .with_min_surfaces(surfaces);
    VideoToolboxDecoderFactory::new()
        .create_session(&config)
        .unwrap()
}

fn surface_of(frame: &VideoFrame) -> usize {
    match frame.payload() {
        FramePayload::Native {
            surface: NativeSurface::IoSurface { surface, .. },
            ..
        } => *surface,
        other => panic!("not an IOSurface: {other:?}"),
    }
}

/// Draws `imported` over the whole canvas and returns the centre and a corner texel.
fn draw(
    ctx: &GpuContext,
    pool: &mut TexturePool,
    comp: &mut Compositor,
    canvas: TextureId,
    imported: &VideoFrame,
) -> [[f32; 4]; 2] {
    let FramePayload::Texture(id) = *imported.payload() else {
        panic!("import must give a texture frame");
    };
    let mut layer = Layer::new(id, 0.0, 0.0, W as f32, H as f32);
    layer.ycbcr = imported.format().color;
    comp.render(ctx, pool, &[layer], canvas).unwrap();
    let px = readback::read_rgba16f(ctx, pool, canvas).unwrap();
    let at = |x: u32, y: u32| px[(y * W + x) as usize];
    [at(W / 2, H / 2), at(4, 4)]
}

fn assert_rgb(got: [f32; 4], want: [f32; 3], what: &str) {
    for c in 0..3 {
        assert!(
            (got[c] - want[c]).abs() <= TOLERANCE,
            "{what}: got {got:?}, want {want:?}"
        );
    }
}

/// Returns every lease the importer holds (it hands them back after the GPU is done).
fn hand_back(ctx: &GpuContext, pool: &mut TexturePool, importer: &mut Importer) {
    importer.retire(ctx, pool, Timestamp::ZERO);
    ctx.wait_idle().unwrap();
    importer.retire(ctx, pool, Timestamp::ZERO);
}

#[test]
fn native_pictures_import_draw_and_return_their_surfaces() {
    let ctx = match GpuContext::new(&GpuRequest::default()) {
        Ok(ctx) => ctx,
        Err(e) => return skip(&format!("no GPU context ({e})")),
    };
    let packets = match decode_support::flat_clip(W, H, &[RED, BLUE], 8) {
        Ok(p) => p,
        Err(e) => return skip(&format!("no H.264 encoder ({e})")),
    };
    let mut d = decoder(3);
    let mut pool = TexturePool::new();
    let mut importer = Importer::new();
    let mut comp = Compositor::new(
        &ctx,
        CanvasDesc {
            width: W,
            height: H,
        },
        1,
    )
    .unwrap();
    let canvas = pool.acquire(&ctx, &TextureDesc::canvas(W, H)).unwrap();

    // Picture 0 (red) is imported and drawn, and its lease is kept by the importer.
    let mut out = Vec::new();
    d.submit(&packets[0]).unwrap();
    d.drain(&mut out).unwrap();
    assert_eq!(out.len(), 1);
    let first = out.remove(0);
    assert_eq!(first.format().pixel_format, PixelFormat::Nv12);
    assert_eq!(first.format().size, Size::new(W, H));
    let first_surface = surface_of(&first);
    let red = importer.import(&ctx, &mut pool, first).unwrap();
    eprintln!("decoded colour space: {:?}", red.format().color);
    let texels = draw(&ctx, &mut pool, &mut comp, canvas, &red);
    eprintln!("picture 0 (red) centre and corner: {texels:?}");
    for px in texels {
        assert_rgb(px, RED_RGB, "picture 0");
    }

    // Two more pictures (blue, red) fill the other slots while picture 0 is leased:
    // neither is written into picture 0's surface, which still draws red.
    let mut held = Vec::new();
    for p in &packets[1..3] {
        d.submit(p).unwrap();
        d.drain(&mut held).unwrap();
    }
    assert_eq!(held.len(), 2);
    assert!(
        held.iter().all(|f| surface_of(f) != first_surface),
        "a leased surface was reused"
    );
    for px in draw(&ctx, &mut pool, &mut comp, canvas, &red) {
        assert_rgb(px, RED_RGB, "picture 0 while pictures 1 and 2 are decoded");
    }
    let blue = importer.import(&ctx, &mut pool, held.remove(0)).unwrap();
    for px in draw(&ctx, &mut pool, &mut comp, canvas, &blue) {
        assert_rgb(px, BLUE_RGB, "picture 1");
    }

    // Every slot is leased now (two in the importer, one held here): picture 3 drops.
    d.submit(&packets[3]).unwrap();
    d.drain(&mut held).unwrap();
    assert_eq!(held.len(), 1, "no new picture while all slots are leased");
    assert_eq!(d.stats().dropped_no_surface, 1);

    // The importer hands its leases back once the GPU is done; then the slots return.
    drop((red, blue));
    held.clear();
    hand_back(&ctx, &mut pool, &mut importer);
    assert_eq!(importer.held_leases(), 0);
    let mut again = Vec::new();
    for p in &packets[4..7] {
        d.submit(p).unwrap();
        d.drain(&mut again).unwrap();
    }
    assert_eq!(again.len(), 3, "the three slots are free again");
    // Picture 4 is red, 5 blue, 6 red.
    for (frame, want) in again.into_iter().zip([RED_RGB, BLUE_RGB, RED_RGB]) {
        let imported = importer.import(&ctx, &mut pool, frame).unwrap();
        for px in draw(&ctx, &mut pool, &mut comp, canvas, &imported) {
            assert_rgb(px, want, "a reused slot");
        }
    }
    hand_back(&ctx, &mut pool, &mut importer);
    assert_eq!(importer.held_leases(), 0);
    assert_eq!(d.stats().cpu_copies, 0);
}

/// Pictures of the churn clip: 10 s of 1080p30.
const CHURN_FRAMES: usize = 330;
/// Pictures decoded while all 8 slots are held: 2 s at 30 fps.
const HOLD_FRAMES: usize = 60;

/// Pool and importer churn at 1080p: every picture is imported and drawn, its lease
/// handed back after the GPU is done (as the studio does), and for 2 s of video every
/// slot is held. VideoToolbox picks the output surfaces itself, so this checks that the
/// set it cycles through stays small: distinct IOSurface addresses and importer opens
/// at most 16. Prints the numbers for the spec note.
#[test]
#[ignore = "slow: encodes and decodes 11 s of 1080p through the Metal importer"]
fn slow_pool_and_importer_churn_stays_bounded() {
    use std::collections::HashSet;

    let ctx = match GpuContext::new(&GpuRequest::default()) {
        Ok(ctx) => ctx,
        Err(e) => return skip(&format!("no GPU context ({e})")),
    };
    let (w, h) = (1920, 1080);
    let packets = match decode_support::clip(
        w,
        h,
        CHURN_FRAMES,
        decode_support::FIRST,
        decode_support::STEP_NS,
    ) {
        Ok(p) => p,
        Err(e) => return skip(&format!("no H.264 encoder ({e})")),
    };
    let mut d = decoder(8);
    let mut pool = TexturePool::new();
    let mut importer = Importer::new();
    let mut comp = Compositor::new(
        &ctx,
        CanvasDesc {
            width: W,
            height: H,
        },
        1,
    )
    .unwrap();
    let canvas = pool.acquire(&ctx, &TextureDesc::canvas(W, H)).unwrap();
    let opened_before = ctx.metrics().surfaces_opened();
    let mut surfaces = HashSet::new();
    let mut out = Vec::new();
    let mut held = Vec::new();
    let hold = 100..100 + HOLD_FRAMES;
    for (i, p) in packets.iter().enumerate() {
        d.submit(p).unwrap_or_else(|e| panic!("packet {i}: {e}"));
        d.drain(&mut out)
            .unwrap_or_else(|e| panic!("drain {i}: {e}"));
        for frame in out.drain(..) {
            surfaces.insert(surface_of(&frame));
            if hold.contains(&i) && held.len() < 8 {
                held.push(frame); // keep the lease, as a stalled consumer would
                continue;
            }
            let imported = importer.import(&ctx, &mut pool, frame).unwrap();
            draw(&ctx, &mut pool, &mut comp, canvas, &imported);
        }
        importer.retire(&ctx, &mut pool, Timestamp::ZERO);
        if i == hold.end {
            held.clear();
        }
    }
    hand_back(&ctx, &mut pool, &mut importer);
    let opened = ctx.metrics().surfaces_opened() - opened_before;
    let st = d.stats();
    eprintln!(
        "churn: {} packets, {} pictures, {} dropped while held, {} distinct IOSurfaces, {} importer opens, {} misuses",
        packets.len(),
        st.pictures,
        st.dropped_no_surface,
        surfaces.len(),
        opened,
        d.lease_misuses()
    );
    assert_eq!(importer.held_leases(), 0);
    assert_eq!(d.lease_misuses(), 0);
    assert!(st.dropped_no_surface > 0, "the hold phase filled the pool");
    assert!(surfaces.len() <= 16, "{} distinct surfaces", surfaces.len());
    assert!(opened <= 16, "{opened} importer opens");
}
