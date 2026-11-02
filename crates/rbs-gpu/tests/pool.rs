//! Texture pool invariants (spec docs/specs/gpu-compositor.md, "Texture pool").

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod common;

use rbs_core::media::{ColorSpace, FramePayload, PixelFormat, Size, VideoFormat};
use rbs_core::time::Timestamp;
use rbs_gpu::{GpuError, TextureDesc, TexturePool};

#[test]
fn new_pool_owns_nothing() {
    let pool = TexturePool::new();
    assert_eq!(pool.allocated(), 0);
}

#[test]
fn release_then_acquire_reuses_texture() {
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::new();
    let desc = TextureDesc::canvas(64, 32);
    let a = pool.acquire(ctx, &desc).unwrap();
    pool.release(a).unwrap();
    let b = pool.acquire(ctx, &desc).unwrap();
    assert_eq!(a.slot(), b.slot(), "same slot reused");
    assert_ne!(a, b, "generation changed");
    assert_eq!(pool.allocated(), 1);
}

#[test]
fn different_description_gets_new_texture() {
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::new();
    let a = pool.acquire(ctx, &TextureDesc::canvas(64, 32)).unwrap();
    pool.release(a).unwrap();
    let b = pool.acquire(ctx, &TextureDesc::canvas(32, 32)).unwrap();
    assert_ne!(a.slot(), b.slot());
    assert_eq!(pool.allocated(), 2);
}

#[test]
fn stale_ids_are_detected() {
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::new();
    let a = pool.acquire(ctx, &TextureDesc::canvas(8, 8)).unwrap();
    pool.release(a).unwrap();
    assert!(pool.get(a).is_none());
    assert_eq!(pool.release(a), Err(GpuError::StaleTexture(a)));
    let b = pool.acquire(ctx, &TextureDesc::canvas(8, 8)).unwrap();
    assert!(pool.get(a).is_none(), "old id stays stale after slot reuse");
    assert!(pool.get(b).is_some());
}

#[test]
fn trim_releases_free_textures_only() {
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::new();
    let desc = TextureDesc::canvas(8, 8);
    let kept = pool.acquire(ctx, &desc).unwrap();
    let freed = pool.acquire(ctx, &desc).unwrap();
    pool.release(freed).unwrap();
    pool.trim();
    assert_eq!(pool.allocated(), 1);
    assert!(pool.get(kept).is_some());
    // The kept texture can still be released and reused after a trim.
    pool.release(kept).unwrap();
    let again = pool.acquire(ctx, &desc).unwrap();
    assert_eq!(again.slot(), kept.slot());
    pool.release(again).unwrap();
    pool.trim();
    assert_eq!(pool.allocated(), 0);
}

#[test]
fn invalid_sizes_are_rejected() {
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::new();
    assert!(matches!(
        pool.acquire(ctx, &TextureDesc::canvas(0, 8)),
        Err(GpuError::InvalidSize { .. })
    ));
    assert!(matches!(
        pool.acquire(ctx, &TextureDesc::canvas(1 << 20, 8)),
        Err(GpuError::InvalidSize { .. })
    ));
}

#[test]
fn lease_keeps_released_slot_alive_until_dropped() {
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::new();
    let desc = TextureDesc::canvas(8, 8);
    let id = pool.acquire(ctx, &desc).unwrap();
    let lease = pool.lease(id).unwrap();
    pool.release(id).unwrap();
    assert!(
        pool.get(id).is_some(),
        "lease holders can still resolve the id"
    );
    assert_eq!(
        pool.release(id),
        Err(GpuError::StaleTexture(id)),
        "owner released once"
    );

    let other = pool.acquire(ctx, &desc).unwrap();
    assert_ne!(other.slot(), id.slot(), "leased slot is not reused");

    // Drop the lease on another thread, as an encoder would.
    std::thread::spawn(move || drop(lease)).join().unwrap();
    pool.release(other).unwrap();
    let again = pool.acquire(ctx, &desc).unwrap();
    let again2 = pool.acquire(ctx, &desc).unwrap();
    assert!(
        [again.slot(), again2.slot()].contains(&id.slot()),
        "slot reused after the last lease is dropped"
    );
    assert!(pool.get(id).is_none(), "old id is stale after reuse");
    assert_eq!(pool.allocated(), 2);
}

#[test]
fn frames_share_the_lease() {
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::new();
    let desc = TextureDesc::canvas(8, 8);
    let id = pool.acquire(ctx, &desc).unwrap();
    let format = VideoFormat::new(
        Size::new(8, 8),
        PixelFormat::Rgba16Float,
        ColorSpace::LINEAR_BT709,
    );
    let frame = pool.frame(id, format, Timestamp::ZERO).unwrap();
    assert_eq!(frame.payload(), &FramePayload::Texture(id));
    let shared = frame.share().unwrap();
    pool.release(id).unwrap();
    drop(frame);
    assert_eq!(pool.in_use(), 1, "shared frame still holds the slot");
    drop(shared);
    let next = pool.acquire(ctx, &desc).unwrap();
    assert_eq!(next.slot(), id.slot());
    assert_eq!(pool.in_use(), 1);
}

#[test]
fn stale_ids_cannot_be_leased() {
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::new();
    let id = pool.acquire(ctx, &TextureDesc::canvas(8, 8)).unwrap();
    pool.release(id).unwrap();
    assert!(matches!(pool.lease(id), Err(GpuError::StaleTexture(_))));
}

#[test]
fn full_pool_is_an_error() {
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::with_capacity(2);
    let desc = TextureDesc::canvas(8, 8);
    let a = pool.acquire(ctx, &desc).unwrap();
    pool.acquire(ctx, &desc).unwrap();
    assert_eq!(
        pool.acquire(ctx, &desc),
        Err(GpuError::PoolFull { capacity: 2 })
    );
    pool.release(a).unwrap();
    assert!(
        pool.acquire(ctx, &desc).is_ok(),
        "a released slot is reused"
    );
}
