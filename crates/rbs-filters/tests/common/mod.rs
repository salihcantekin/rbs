//! Shared helpers for the filter tests.
#![allow(dead_code, clippy::expect_used)]

use std::sync::{Arc, OnceLock};

use rbs_filters::FilterType;
use rbs_gpu::{GpuContext, GpuError, GpuRequest};

/// One shared context per test binary (software drivers crash with several devices).
pub fn gpu() -> Option<Arc<GpuContext>> {
    static CTX: OnceLock<Option<Arc<GpuContext>>> = OnceLock::new();
    let ctx = CTX.get_or_init(|| {
        match GpuContext::new(&GpuRequest {
            allow_software: true,
            ..GpuRequest::default()
        }) {
            Ok(ctx) => Some(Arc::new(ctx)),
            Err(GpuError::NoAdapter) => None,
            Err(e) => panic!("GPU context failed: {e}"),
        }
    });
    if ctx.is_none() {
        assert!(
            std::env::var_os("RBS_REQUIRE_GPU").is_none_or(|v| v.is_empty()),
            "RBS_REQUIRE_GPU is set but no GPU adapter was found"
        );
        eprintln!("SKIPPED: no GPU adapter");
    }
    ctx.clone()
}

/// A catalogue entry as an owned `Arc<dyn FilterType>`.
pub fn shared(ty: &'static dyn FilterType) -> Arc<dyn FilterType> {
    Arc::new(ty)
}
