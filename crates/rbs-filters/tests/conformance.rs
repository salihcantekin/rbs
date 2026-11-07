//! Every built-in filter type passes the conformance suite (spec `video-filters.md`).
#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod common;

use rbs_filters::catalogue;
use rbs_filters::conformance::{check_on_gpu, check_type};

#[test]
fn catalogue_types_pass_the_pure_checks() {
    for ty in catalogue() {
        let problems = check_type(*ty);
        assert!(problems.is_empty(), "{}: {problems:#?}", ty.name());
    }
}

#[test]
fn catalogue_types_pass_on_the_gpu() {
    let Some(ctx) = common::gpu() else { return };
    for ty in catalogue() {
        let problems = check_on_gpu(common::shared(*ty), ctx.clone());
        assert!(problems.is_empty(), "{}: {problems:#?}", ty.name());
    }
}
