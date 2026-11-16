//! What the caller tells the importer about this build and this computer (ADR-0131
//! decision 2). The importer never probes devices or opens pictures itself.

use std::path::Path;

use rbs_graph::node::NodeTypeId;
use rbs_params::{Field, FieldKind};

/// The RBS types this build has, with their field schemas.
pub trait Catalogue {
    /// True when the source, filter or transition type exists in this build.
    fn has(&self, type_id: &NodeTypeId) -> bool;
    /// The fields of a filter type, used to clamp imported values; `None` when the type
    /// has no schema here (sources keep their settings as mapped).
    fn fields(&self, type_id: &NodeTypeId) -> Option<&'static [Field]>;
}

/// A capture target or audio endpoint on this computer.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LookupTarget {
    /// The adapter that offers it; stored with the target.
    pub adapter: String,
    /// The target id.
    pub target: String,
    /// What the user sees: a device, display or window title.
    pub name: String,
    /// The owning application, when the OS tells it.
    pub app: Option<String>,
    /// Picture size in pixels, when known.
    pub size: Option<(u32, u32)>,
}

impl LookupTarget {
    /// A target with no application and no size.
    pub fn new(
        adapter: impl Into<String>,
        target: impl Into<String>,
        name: impl Into<String>,
    ) -> Self {
        Self {
            adapter: adapter.into(),
            target: target.into(),
            name: name.into(),
            app: None,
            size: None,
        }
    }

    /// The same target with an application.
    #[must_use]
    pub fn with_app(mut self, app: impl Into<String>) -> Self {
        self.app = Some(app.into());
        self
    }

    /// The same target with a picture size.
    #[must_use]
    pub fn with_size(mut self, w: u32, h: u32) -> Self {
        self.size = Some((w, h));
        self
    }
}

/// Facts about files and devices on this computer.
pub trait Lookup {
    /// Width and height of a PNG file, or `None` when it is missing or not a PNG.
    fn picture_size(&self, path: &Path) -> Option<(u32, u32)>;
    /// True when the file exists.
    fn file_exists(&self, path: &Path) -> bool;
    /// The targets of a source type. Asked at most once per type per import.
    fn targets(&self, type_id: &NodeTypeId) -> Vec<LookupTarget>;
}

/// A lookup that knows nothing: no files, no devices. Every target falls back to the
/// default and every picture size to the canvas, marked approximate.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoLookup;

impl Lookup for NoLookup {
    fn picture_size(&self, _: &Path) -> Option<(u32, u32)> {
        None
    }
    fn file_exists(&self, _: &Path) -> bool {
        false
    }
    fn targets(&self, _: &NodeTypeId) -> Vec<LookupTarget> {
        Vec::new()
    }
}

/// `value` clamped to the number field `key` of `fields`, and whether it was clamped.
/// A missing field or a field that is not a number leaves the value unchanged.
pub fn clamp_field(fields: Option<&[Field]>, key: &str, value: f64) -> (f64, bool) {
    let Some(field) = fields.and_then(|f| f.iter().find(|f| f.key == key)) else {
        return (value, false);
    };
    match field.kind {
        FieldKind::Number { min, max, .. } => {
            let c = value.clamp(f64::from(min), f64::from(max));
            (c, (c - value).abs() > 1e-9)
        }
        _ => (value, false),
    }
}
