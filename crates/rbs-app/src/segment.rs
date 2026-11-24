//! Background removal in this build (ADR-0179): which person mask backends exist and
//! the filter type they serve. A composition root: the only place that names a backend.

use std::sync::Arc;

use rbs_filters::FilterType;
use rbs_filters_segment::BACKGROUND_REMOVAL;
use rbs_segment::PersonMaskBackend;

static EXTRA: [&dyn FilterType; 1] = [&BACKGROUND_REMOVAL];

/// Installs the backends and adds the filter to the catalogue. Call once at start-up,
/// before anything reads the filter catalogue. The filter is offered only where a
/// backend can run (`rbs_filters_segment::available`).
pub fn install() {
    // In order of preference: the OS backend first. Windows and the bundled network
    // follow with their measurements (ADR-0179 decision 13).
    let backends: Vec<Arc<dyn PersonMaskBackend>> =
        vec![Arc::new(rbs_segment_vision::VisionBackend::new())];
    rbs_filters_segment::install_backends(backends);
    rbs_filters::install_extra(&EXTRA);
}
