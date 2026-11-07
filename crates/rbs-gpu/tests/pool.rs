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

#[test]
fn trim_idle_drops_only_descriptions_nobody_acquired_lately() {
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::new();
    let busy = TextureDesc::canvas(16, 16);
    let gone = TextureDesc::canvas(8, 8);
    let old = pool.acquire(ctx, &gone).unwrap();
    pool.release(old).unwrap();
    for _ in 0..=10 {
        let a = pool.acquire(ctx, &busy).unwrap();
        pool.release(a).unwrap();
        pool.trim_idle(5);
    }
    // `gone` was last asked for at tick 0, `busy` every tick.
    assert_eq!(pool.allocated(), 1);
    let again = pool.acquire(ctx, &busy).unwrap();
    assert_eq!(pool.allocated(), 1, "the busy texture was kept and reused");
    // A texture in use is never dropped, however long it is held.
    for _ in 11..40 {
        pool.trim_idle(5);
    }
    assert_eq!(pool.allocated(), 1);
    pool.release(again).unwrap();
    pool.trim_idle(5);
    assert_eq!(pool.allocated(), 0);
    assert_eq!(pool.descriptions(), 0, "empty descriptions are forgotten");
}

#[test]
fn touch_keeps_a_skipping_users_textures() {
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::new();
    let desc = TextureDesc::canvas(8, 8);
    let a = pool.acquire(ctx, &desc).unwrap();
    pool.release(a).unwrap();
    for _ in 0..20 {
        pool.touch(&desc);
        pool.trim_idle(5);
    }
    assert_eq!(pool.allocated(), 1);
}

#[test]
fn a_full_pool_evicts_the_least_recently_used_free_textures() {
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::with_capacity(3);
    let (old, newer, wanted) = (
        TextureDesc::canvas(4, 4),
        TextureDesc::canvas(5, 5),
        TextureDesc::canvas(6, 6),
    );
    let a = pool.acquire(ctx, &old).unwrap();
    pool.release(a).unwrap();
    pool.trim_idle(100);
    let b = pool.acquire(ctx, &newer).unwrap();
    pool.release(b).unwrap();
    let held = pool.acquire(ctx, &wanted).unwrap();
    // Full: `old` (asked for at tick 0) goes first, `newer` stays.
    let c = pool.acquire(ctx, &wanted).unwrap();
    assert_eq!(pool.allocated(), 3);
    assert_eq!(pool.descriptions(), 2);
    let again = pool.acquire(ctx, &newer).unwrap();
    assert_eq!(again.slot(), b.slot(), "newer was kept");
    // Every texture in use: nothing to evict.
    assert!(matches!(
        pool.acquire(ctx, &old),
        Err(GpuError::PoolFull { capacity: 3 })
    ));
    for id in [held, c, again] {
        pool.release(id).unwrap();
    }
}

#[test]
fn forget_drops_free_textures_now_and_a_forgotten_leased_one_when_freed() {
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::new();
    let desc = TextureDesc::canvas(8, 8);
    let format = VideoFormat::new(
        Size::new(8, 8),
        PixelFormat::Rgba16Float,
        ColorSpace::LINEAR_BT709,
    );
    // One user's two textures (one still read) and another user's, released first.
    let leased = pool.acquire(ctx, &desc).unwrap();
    let spare = pool.acquire(ctx, &desc).unwrap();
    let other = pool.acquire(ctx, &desc).unwrap();
    let frame = pool.frame(leased, format, Timestamp::ZERO).unwrap();
    pool.release(other).unwrap();
    pool.release(leased).unwrap();
    pool.release(spare).unwrap();
    assert_eq!(pool.allocated(), 3);
    pool.forget_texture(leased);
    pool.forget(&desc, 1);
    assert_eq!(pool.allocated(), 2, "one free texture is dropped at once");
    drop(frame);
    pool.trim_idle(1000);
    assert_eq!(
        pool.allocated(),
        1,
        "the forgotten one is dropped when its reader lets go"
    );
    // The other user still finds a texture of its own.
    let again = pool.acquire(ctx, &desc).unwrap();
    assert_eq!(pool.allocated(), 1);
    pool.release(again).unwrap();
    // A stale id does nothing.
    pool.forget_texture(leased);
    assert_eq!(pool.allocated(), 1);
}

#[test]
fn a_full_pool_never_evicts_what_was_asked_for_this_tick() {
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::with_capacity(2);
    let (a, b) = (TextureDesc::canvas(4, 4), TextureDesc::canvas(5, 5));
    let x = pool.acquire(ctx, &a).unwrap();
    pool.release(x).unwrap();
    // `a` was asked for in this tick, so `b` cannot take its slot.
    let y = pool.acquire(ctx, &b).unwrap();
    assert!(matches!(
        pool.acquire(ctx, &b),
        Err(GpuError::PoolFull { capacity: 2 })
    ));
    pool.release(y).unwrap();
    // After the tick, `a` is evictable.
    pool.trim_idle(100);
    let held = pool.acquire(ctx, &b).unwrap();
    let z = pool.acquire(ctx, &b).unwrap();
    assert_eq!(pool.allocated(), 2);
    pool.release(held).unwrap();
    pool.release(z).unwrap();
}

#[test]
fn a_native_import_into_a_full_pool_evicts_idle_textures() {
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::with_capacity(1);
    let idle = pool.acquire(ctx, &TextureDesc::canvas(4, 4)).unwrap();
    pool.release(idle).unwrap();
    pool.trim_idle(100);
    let desc = TextureDesc::canvas(8, 8);
    let texture = ctx.device().create_texture(&wgpu::TextureDescriptor {
        label: Some("imported"),
        size: wgpu::Extent3d {
            width: 8,
            height: 8,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: desc.format,
        usage: desc.usage,
        view_formats: &[],
    });
    let id = pool.insert_external(ctx, texture, desc).unwrap();
    assert_eq!(pool.allocated(), 1, "the idle texture made room");
    assert_eq!(pool.descriptions(), 0);
    pool.release(id).unwrap();
}
