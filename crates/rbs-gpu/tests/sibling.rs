//! `GpuContext::sibling` and `wait_until` (ADR-0087 step D2: the throwaway device of
//! *Test this computer*). Runs on any adapter, software included; skips when there is
//! none (set `RBS_REQUIRE_GPU` to fail instead).

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod common;

use std::time::Duration;

#[test]
fn a_sibling_is_a_working_device_on_the_same_adapter_and_waits_are_bounded() {
    let Some(ctx) = common::gpu() else { return };
    let sibling = ctx.sibling().unwrap();
    assert_eq!(sibling.adapter_key(), ctx.adapter_key());
    assert_eq!(sibling.capabilities(), ctx.capabilities());
    // Some work on the sibling, then a bounded wait that finishes.
    let buffer = sibling.device().create_buffer(&wgpu::BufferDescriptor {
        label: Some("sibling test"),
        size: 256,
        usage: wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    sibling.queue().write_buffer(&buffer, 0, &[1; 256]);
    sibling.queue().submit(None);
    assert!(sibling.wait_until(Duration::from_secs(10)).unwrap());
    // Nothing submitted: the wait returns at once.
    assert!(ctx.wait_until(Duration::from_millis(1)).unwrap());
    assert!(!sibling.metrics().device_lost());
}
