//! Library operations shared by local menus and external registry resolution.

/// The two libraries the status bar's pickers switch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    Profile,
    Collection,
}

/// What an Output profile or Scene collection command does to the one in use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryOp {
    New,
    Duplicate,
    Rename,
    Delete,
    Import,
    Export,
}

impl EntryOp {
    /// The word the registry ids end in.
    pub fn name(self) -> &'static str {
        match self {
            EntryOp::New => "new",
            EntryOp::Duplicate => "duplicate",
            EntryOp::Rename => "rename",
            EntryOp::Delete => "delete",
            EntryOp::Import => "import",
            EntryOp::Export => "export",
        }
    }
}

/// The library and operation of a `ui.profile.*` or `ui.collection.*` id.
pub(super) fn entry_op(base: &str) -> Option<(EntryKind, EntryOp)> {
    let (kind, op) = if let Some(op) = base.strip_prefix("ui.profile.") {
        (EntryKind::Profile, op)
    } else {
        (EntryKind::Collection, base.strip_prefix("ui.collection.")?)
    };
    let op = [
        EntryOp::New,
        EntryOp::Duplicate,
        EntryOp::Rename,
        EntryOp::Delete,
        EntryOp::Import,
        EntryOp::Export,
    ]
    .into_iter()
    .find(|o| o.name() == op)?;
    Some((kind, op))
}
