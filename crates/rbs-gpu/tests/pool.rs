//! Texture pool invariants (spec docs/specs/gpu-compositor.md, "Texture pool").

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod common;

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
    let a = pool.acquire(&ctx, &desc).unwrap();
    pool.release(a).unwrap();
    let b = pool.acquire(&ctx, &desc).unwrap();
    assert_eq!(a.index(), b.index(), "same slot reused");
    assert_ne!(a, b, "generation changed");
    assert_eq!(pool.allocated(), 1);
}

#[test]
fn different_description_gets_new_texture() {
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::new();
    let a = pool.acquire(&ctx, &TextureDesc::canvas(64, 32)).unwrap();
    pool.release(a).unwrap();
    let b = pool.acquire(&ctx, &TextureDesc::canvas(32, 32)).unwrap();
    assert_ne!(a.index(), b.index());
    assert_eq!(pool.allocated(), 2);
}

#[test]
fn stale_ids_are_detected() {
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::new();
    let a = pool.acquire(&ctx, &TextureDesc::canvas(8, 8)).unwrap();
    pool.release(a).unwrap();
    assert!(pool.get(a).is_none());
    assert_eq!(pool.release(a), Err(GpuError::StaleTexture(a)));
    let b = pool.acquire(&ctx, &TextureDesc::canvas(8, 8)).unwrap();
    assert!(pool.get(a).is_none(), "old id stays stale after slot reuse");
    assert!(pool.get(b).is_some());
}

#[test]
fn trim_releases_free_textures_only() {
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::new();
    let desc = TextureDesc::canvas(8, 8);
    let kept = pool.acquire(&ctx, &desc).unwrap();
    let freed = pool.acquire(&ctx, &desc).unwrap();
    pool.release(freed).unwrap();
    pool.trim();
    assert_eq!(pool.allocated(), 1);
    assert!(pool.get(kept).is_some());
    // The kept texture can still be released and reused after a trim.
    pool.release(kept).unwrap();
    let again = pool.acquire(&ctx, &desc).unwrap();
    assert_eq!(again.index(), kept.index());
    pool.release(again).unwrap();
    pool.trim();
    assert_eq!(pool.allocated(), 0);
}

#[test]
fn invalid_sizes_are_rejected() {
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::new();
    assert!(matches!(
        pool.acquire(&ctx, &TextureDesc::canvas(0, 8)),
        Err(GpuError::InvalidSize { .. })
    ));
    assert!(matches!(
        pool.acquire(&ctx, &TextureDesc::canvas(1 << 20, 8)),
        Err(GpuError::InvalidSize { .. })
    ));
}
