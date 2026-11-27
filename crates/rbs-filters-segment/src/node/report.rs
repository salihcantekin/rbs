//! Control-thread log lines of the background removal node (child of `node`, which is a
//! hot file and logs nothing itself, ADR-0158 decision 3).

use std::time::Duration;

use rbs_core::log::Area;

/// The session was opened and warmed in `prepare`.
pub(super) fn ready(took: Duration) {
    rbs_core::info!(
        Area::Engine,
        "background removal session ready in {} ms",
        took.as_millis()
    );
}

/// The node left the schedule (logged in `release`): whether it joined on a turn-on,
/// the ticks it published nothing and the plain picture before its first mask, and how
/// long after its first tick that mask came.
pub(super) fn left(turned_on: bool, blank: u32, plain: u32, first_mask_ms: Option<u64>) {
    let first = first_mask_ms.map_or_else(|| "none".to_owned(), |ms| format!("{ms} ms"));
    rbs_core::info!(
        Area::Engine,
        "background removal left the schedule: turned on {turned_on}, {blank} blank and \
         {plain} plain ticks before the first mask, first mask after {first}"
    );
}
