//! The studio can use what the decoder hands out: native NV12 pictures of the Media
//! Foundation decoder are imported through `rbs-gpu`'s D3D12 importer (as the media
//! picture node does), drawn by the compositor through its 4:2:0 path and read back.
//! Also checks that a leased surface is not written while the lease lives, and that a
//! slot comes back for reuse after the importer hands the lease back.
//!
//! Skips with a reason when the render device is not D3D12 or the decoder MFT refuses
//! DXVA; `RBS_REQUIRE_MF_H264=1` turns the skips into failures.
#![cfg(windows)]
#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use rbs_codec::{DecodeOutput, VideoDecoder, VideoDecoderConfig};
use rbs_codec_mf::MfH264DecoderFactory;
use rbs_codec_mf::decode::MfH264Decoder;
use rbs_codec_mf::testing::h264;
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
    std::env::var("RBS_REQUIRE_MF_H264").is_ok_and(|v| v == "1")
}

fn skip(why: &str) {
    assert!(!required(), "RBS_REQUIRE_MF_H264=1 but {why}");
    eprintln!("SKIPPED: {why}");
}

struct Studio {
    ctx: GpuContext,
    luid: u64,
}

/// A hardware render device on D3D12, as the studio uses.
fn studio() -> Option<Studio> {
    let ctx = match GpuContext::new(&GpuRequest::default()) {
        Ok(ctx) => ctx,
        Err(e) => {
            skip(&format!("no GPU context ({e})"));
            return None;
        }
    };
    let Some(luid) = ctx.adapter_luid() else {
        skip("the render device is not D3D12");
        return None;
    };
    Some(Studio { ctx, luid })
}

fn decoder(luid: u64, surfaces: usize) -> Option<MfH264Decoder> {
    let target = DecodeOutput::system()
        .accepting(MemoryKind::D3d11)
        .on_adapter(luid);
    let config = VideoDecoderConfig::h264_annex_b(Size::new(1920, 1088))
        .with_target(target)
        .with_min_surfaces(surfaces);
    let d = MfH264DecoderFactory::new().create_session(&config).ok()?;
    if !d.is_native() {
        skip("the decoder MFT refused DXVA on the render adapter");
        return None;
    }
    Some(d)
}

fn handle_of(frame: &VideoFrame) -> u64 {
    match frame.payload() {
        FramePayload::Native {
            surface: NativeSurface::D3dSharedHandle { handle, .. },
            ..
        } => *handle,
        other => panic!("not a shared D3D handle: {other:?}"),
    }
}

/// Draws `texture` over the whole canvas and returns the centre and a corner texel.
fn draw(
    s: &Studio,
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
    comp.render(&s.ctx, pool, &[layer], canvas).unwrap();
    let px = readback::read_rgba16f(&s.ctx, pool, canvas).unwrap();
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
fn hand_back(s: &Studio, pool: &mut TexturePool, importer: &mut Importer) {
    importer.retire(&s.ctx, pool, Timestamp::ZERO);
    s.ctx.wait_idle().unwrap();
    importer.retire(&s.ctx, pool, Timestamp::ZERO);
}

#[test]
fn native_pictures_import_draw_and_return_their_surfaces() {
    let Some(s) = studio() else { return };
    let packets = match h264::flat_clip(W, H, &[RED, BLUE], 8, Timestamp::from_nanos(9), 33_333_337)
    {
        Ok(p) => p,
        Err(e) => return skip(&format!("no H.264 encoder ({e})")),
    };
    let Some(mut d) = decoder(s.luid, 3) else {
        return;
    };
    let mut pool = TexturePool::new();
    let mut importer = Importer::new();
    let mut comp = Compositor::new(
        &s.ctx,
        CanvasDesc {
            width: W,
            height: H,
        },
        1,
    )
    .unwrap();
    let canvas = pool.acquire(&s.ctx, &TextureDesc::canvas(W, H)).unwrap();

    // Picture 0 (red) is imported and drawn, and its lease is kept by the importer.
    let mut out = Vec::new();
    d.submit(&packets[0]).unwrap();
    d.drain(&mut out).unwrap();
    assert_eq!(out.len(), 1);
    let first = out.remove(0);
    assert_eq!(first.format().pixel_format, PixelFormat::Nv12);
    assert_eq!(first.format().size, Size::new(W, H));
    let first_handle = handle_of(&first);
    let red = importer.import(&s.ctx, &mut pool, first).unwrap();
    eprintln!("decoded colour space: {:?}", red.format().color);
    let texels = draw(&s, &mut pool, &mut comp, canvas, &red);
    eprintln!("picture 0 (red) centre and corner: {texels:?}");
    for px in texels {
        assert_rgb(px, RED_RGB, "picture 0");
    }

    // Two more pictures (blue, red) fill the other slots while picture 0 is leased:
    // neither is written into picture 0's texture, which still draws red.
    let mut held = Vec::new();
    for p in &packets[1..3] {
        d.submit(p).unwrap();
        d.drain(&mut held).unwrap();
    }
    assert_eq!(held.len(), 2);
    let handles: Vec<u64> = held.iter().map(handle_of).collect();
    assert!(!handles.contains(&first_handle), "a leased slot was reused");
    for px in draw(&s, &mut pool, &mut comp, canvas, &red) {
        assert_rgb(px, RED_RGB, "picture 0 while picture 1 and 2 are decoded");
    }
    let blue = importer.import(&s.ctx, &mut pool, held.remove(0)).unwrap();
    for px in draw(&s, &mut pool, &mut comp, canvas, &blue) {
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
    hand_back(&s, &mut pool, &mut importer);
    assert_eq!(importer.held_leases(), 0);
    let mut again = Vec::new();
    for p in &packets[4..7] {
        d.submit(p).unwrap();
        d.drain(&mut again).unwrap();
    }
    assert_eq!(again.len(), 3, "the three slots are free again");
    let mut reused: Vec<u64> = again.iter().map(handle_of).collect();
    let mut known = handles.clone();
    known.push(first_handle);
    reused.sort_unstable();
    known.sort_unstable();
    assert_eq!(reused, known, "the same three textures, no new allocation");
    // Picture 4 is red, 5 blue, 6 red: the reused textures hold the new pictures.
    for (frame, want) in again.into_iter().zip([RED_RGB, BLUE_RGB, RED_RGB]) {
        let imported = importer.import(&s.ctx, &mut pool, frame).unwrap();
        for px in draw(&s, &mut pool, &mut comp, canvas, &imported) {
            assert_rgb(px, want, "a reused slot");
        }
    }
    hand_back(&s, &mut pool, &mut importer);
    assert_eq!(importer.held_leases(), 0);
}
