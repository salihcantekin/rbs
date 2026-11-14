//! Shared helpers for GPU integration tests.

#![allow(dead_code, clippy::expect_used)] // Not every test binary uses every helper; tests fail by panicking.

use std::cell::RefCell;
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

use rbs_gpu::{GpuContext, GpuError, GpuRequest, TextureDesc, TextureId, TexturePool, readback};

/// One shared context per test binary, on any adapter (software included).
///
/// Shared because an engine has one device, and because creating and destroying
/// many Vulkan devices concurrently is not what we test (and crashes some software
/// drivers). `None` (with a message) when no adapter exists, so tests skip instead
/// of failing on machines without a GPU.
///
/// The calling test also takes its turn on the device: tests of one binary that call
/// this run one after another, never at once. wgpu runs a buffer's map callback on
/// whichever thread polls the device next, so a test whose `wait_idle` and `poll`
/// returned could still find its own readback not mapped yet, because another test's
/// poll had collected the callback and not run it yet (seen as a rare
/// `a_scaled_download_fits_the_canvas_with_black_bars` failure under `cargo test`,
/// reproduced at about 5 in 10 000 with a second thread polling). The turn is held
/// until the test's thread ends; `cargo nextest` runs each test in its own process
/// and is unaffected. Threads a test spawns must not call this function.
pub fn gpu() -> Option<&'static GpuContext> {
    take_turn();
    static CTX: OnceLock<Option<GpuContext>> = OnceLock::new();
    let ctx = CTX.get_or_init(|| {
        match GpuContext::new(&GpuRequest {
            allow_software: true,
            ..GpuRequest::default()
        }) {
            Ok(ctx) => Some(ctx),
            Err(GpuError::NoAdapter) => None,
            Err(e) => panic!("GPU context failed: {e}"),
        }
    });
    if ctx.is_none() {
        // CI sets this where an adapter is known to exist, so a skip cannot hide a failure.
        assert!(
            std::env::var_os("RBS_REQUIRE_GPU").is_none_or(|v| v.is_empty()),
            "RBS_REQUIRE_GPU is set but no GPU adapter was found"
        );
        eprintln!("SKIPPED: no GPU adapter (install lavapipe or WARP to run GPU tests)");
    }
    ctx.as_ref()
}

/// Takes the calling thread's turn on the shared device (see [`gpu`]); a thread that
/// holds it already keeps it. Released when the thread ends, also after a panic.
fn take_turn() {
    static DEVICE: Mutex<()> = Mutex::new(());
    thread_local! {
        static TURN: RefCell<Option<MutexGuard<'static, ()>>> = const { RefCell::new(None) };
    }
    TURN.with(|turn| {
        let mut turn = turn.borrow_mut();
        if turn.is_none() {
            // A test that panicked while holding the turn poisons the lock; its panic
            // already fails that test, so the next one goes on.
            *turn = Some(DEVICE.lock().unwrap_or_else(PoisonError::into_inner));
        }
    });
}

/// Encodes an `f32` as IEEE half (exact for the simple values tests use).
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

/// A `w x h` `Rgba16Float` source filled with one premultiplied linear colour.
pub fn solid(
    ctx: &GpuContext,
    pool: &mut TexturePool,
    w: u32,
    h: u32,
    rgba: [f32; 4],
) -> TextureId {
    let id = pool
        .acquire(
            ctx,
            &TextureDesc::source(w, h, wgpu::TextureFormat::Rgba16Float),
        )
        .expect("acquire");
    let texel: Vec<u8> = rgba
        .iter()
        .flat_map(|c| f32_to_f16(*c).to_le_bytes())
        .collect();
    let data: Vec<u8> = (0..w * h).flat_map(|_| texel.iter().copied()).collect();
    readback::upload(ctx, pool, id, &data).expect("upload");
    id
}

/// The texel at `(x, y)` of an `Rgba16Float` texture of width `w`.
pub fn at(pixels: &[[f32; 4]], w: u32, x: u32, y: u32) -> [f32; 4] {
    pixels[(y * w + x) as usize]
}

/// Asserts two colours are equal within `tol` per channel.
pub fn assert_close(got: [f32; 4], want: [f32; 4], tol: f32) {
    for i in 0..4 {
        assert!(
            (got[i] - want[i]).abs() <= tol,
            "channel {i}: got {got:?}, want {want:?} (tolerance {tol})"
        );
    }
}
