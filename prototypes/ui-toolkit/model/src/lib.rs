//! Toolkit-neutral panel and workspace model for the UI toolkit prototypes.
//!
//! It implements a subset of the workspace contract in
//! `docs/specs/ui-design-system.md` (panels and workspaces): a layout tree per
//! window whose leaves are tab stacks, floating windows, and a versioned TOML
//! file. Every prototype uses this crate unchanged, so the comparison only
//! measures toolkit glue. The real UI crate would own an equivalent model (P1).

use serde::{Deserialize, Serialize};

/// Panels known to the prototypes: (id, title).
pub const PANELS: &[(&str, &str)] = &[
    ("rbs.program", "Program"),
    ("rbs.scenes", "Scenes"),
    ("rbs.sources", "Sources"),
    ("rbs.mixer", "Mixer"),
    ("rbs.live-controls", "Live controls"),
    ("rbs.stats", "Stats"),
];

/// Title for a panel id, or the id itself for unknown panels.
pub fn title(id: &str) -> &str {
    PANELS
        .iter()
        .find(|(p, _)| *p == id)
        .map(|(_, t)| *t)
        .unwrap_or(id)
}

/// Split direction. `Horizontal` places children side by side.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Dir {
    /// Children left to right.
    Horizontal,
    /// Children top to bottom.
    Vertical,
}

/// A layout tree node.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Node {
    /// A split with one ratio per child (sums to 1).
    Split {
        /// Direction.
        split: Dir,
        /// Ratios.
        ratios: Vec<f32>,
        /// Children.
        children: Vec<Node>,
    },
    /// A tab stack.
    Tabs {
        /// Panel ids.
        tabs: Vec<String>,
        /// Index of the visible tab.
        #[serde(default)]
        active: usize,
    },
}

/// Window kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WindowKind {
    /// The main window.
    Main,
    /// A floating panel window.
    Floating,
}

/// One window and its layout.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Window {
    /// Kind.
    pub kind: WindowKind,
    /// Layout tree.
    pub layout: Node,
}

/// A workspace file (format 1 subset).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Workspace {
    /// Format version.
    pub format: u32,
    /// Display name.
    pub name: String,
    /// Windows; the first is the main window.
    pub windows: Vec<Window>,
}

/// Errors from workspace parsing.
#[derive(Debug)]
pub enum Error {
    /// TOML could not be parsed.
    Parse(String),
    /// Unsupported format version.
    Format(u32),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Parse(e) => write!(f, "invalid workspace file: {e}"),
            Error::Format(v) => write!(f, "unsupported workspace format {v}"),
        }
    }
}

impl std::error::Error for Error {}

fn tabs(ids: &[&str]) -> Node {
    Node::Tabs {
        tabs: ids.iter().map(|s| s.to_string()).collect(),
        active: 0,
    }
}

impl Workspace {
    /// Built-in "Edit" workspace.
    pub fn edit() -> Self {
        Workspace {
            format: 1,
            name: "Edit".into(),
            windows: vec![Window {
                kind: WindowKind::Main,
                layout: Node::Split {
                    split: Dir::Vertical,
                    ratios: vec![0.68, 0.32],
                    children: vec![
                        Node::Split {
                            split: Dir::Horizontal,
                            ratios: vec![0.22, 0.78],
                            children: vec![
                                tabs(&["rbs.scenes", "rbs.sources"]),
                                tabs(&["rbs.program"]),
                            ],
                        },
                        Node::Split {
                            split: Dir::Horizontal,
                            ratios: vec![0.6, 0.4],
                            children: vec![
                                tabs(&["rbs.mixer", "rbs.stats"]),
                                tabs(&["rbs.live-controls"]),
                            ],
                        },
                    ],
                },
            }],
        }
    }

    /// Built-in "Minimal" live workspace.
    pub fn minimal() -> Self {
        Workspace {
            format: 1,
            name: "Minimal".into(),
            windows: vec![Window {
                kind: WindowKind::Main,
                layout: Node::Split {
                    split: Dir::Vertical,
                    ratios: vec![0.8, 0.2],
                    children: vec![
                        tabs(&["rbs.program"]),
                        tabs(&["rbs.live-controls", "rbs.mixer"]),
                    ],
                },
            }],
        }
    }

    /// Parse a workspace file.
    pub fn from_toml(s: &str) -> Result<Self, Error> {
        let w: Workspace = toml::from_str(s).map_err(|e| Error::Parse(e.to_string()))?;
        if w.format != 1 {
            return Err(Error::Format(w.format));
        }
        Ok(w)
    }

    /// Serialize to TOML.
    pub fn to_toml(&self) -> String {
        // Serializing this plain data model cannot fail.
        toml::to_string_pretty(self).unwrap_or_default()
    }

    /// Is the panel open anywhere?
    pub fn is_open(&self, id: &str) -> bool {
        self.windows.iter().any(|w| contains(&w.layout, id))
    }

    /// Ids of all panels that are the active tab of a stack (that is, visible).
    pub fn visible_panels(&self) -> Vec<String> {
        let mut out = Vec::new();
        for w in &self.windows {
            collect_visible(&w.layout, &mut out);
        }
        out
    }

    /// Number of tab stacks in window `win`.
    pub fn stack_count(&self, win: usize) -> usize {
        self.windows
            .get(win)
            .map(|w| count_stacks(&w.layout))
            .unwrap_or(0)
    }

    /// Close a panel wherever it is. Empty stacks and empty floating windows are removed.
    pub fn close(&mut self, id: &str) {
        for w in &mut self.windows {
            remove(&mut w.layout, id);
            prune(&mut w.layout);
        }
        self.windows
            .retain(|w| w.kind == WindowKind::Main || !is_empty(&w.layout));
    }

    /// Open a panel in the first stack of the main window (no-op if open).
    pub fn open(&mut self, id: &str) {
        if self.is_open(id) {
            return;
        }
        if let Some(w) = self.windows.first_mut() {
            if is_empty(&w.layout) {
                w.layout = tabs(&[id]);
            } else if let Some(Node::Tabs { tabs, active }) = nth_stack(&mut w.layout, 0) {
                tabs.push(id.to_string());
                *active = tabs.len() - 1;
            }
        }
    }

    /// Move a panel to the next stack of the main window (wrapping).
    pub fn move_to_next_stack(&mut self, id: &str) {
        let Some(main) = self.windows.first() else {
            return;
        };
        let n = count_stacks(&main.layout);
        let Some(cur) = stack_of(&main.layout, id) else {
            return;
        };
        if n < 2 {
            return;
        }
        let target = (cur + 1) % n;
        let w = &mut self.windows[0];
        // Insert first so stack indices stay valid, then remove the old copy.
        if let Some(Node::Tabs { tabs, active }) = nth_stack(&mut w.layout, target) {
            tabs.push(id.to_string());
            *active = tabs.len() - 1;
        }
        if let Some(Node::Tabs { tabs, active }) = nth_stack(&mut w.layout, cur) {
            if let Some(pos) = tabs.iter().position(|t| t == id) {
                tabs.remove(pos);
                *active = (*active).min(tabs.len().saturating_sub(1));
            }
        }
        prune(&mut w.layout);
    }

    /// Move a panel into a new floating window. Returns the window index.
    pub fn float(&mut self, id: &str) -> Option<usize> {
        if !self.is_open(id) {
            return None;
        }
        self.close(id);
        self.windows.push(Window {
            kind: WindowKind::Floating,
            layout: tabs(&[id]),
        });
        Some(self.windows.len() - 1)
    }

    /// Dock every panel of floating window `win` back into the main window.
    pub fn dock(&mut self, win: usize) {
        if win == 0 || win >= self.windows.len() {
            return;
        }
        let w = self.windows.remove(win);
        let mut ids = Vec::new();
        collect_all(&w.layout, &mut ids);
        for id in ids {
            self.open(&id);
        }
    }

    /// Activate tab `tab` of stack `stack` in window `win`.
    pub fn activate(&mut self, win: usize, stack: usize, tab: usize) {
        if let Some(w) = self.windows.get_mut(win) {
            if let Some(Node::Tabs { tabs, active }) = nth_stack(&mut w.layout, stack) {
                if tab < tabs.len() {
                    *active = tab;
                }
            }
        }
    }

    /// Flatten window `win` into absolute stack rects and splitter handles.
    pub fn flatten(&self, win: usize, rect: Rect) -> Flat {
        let mut flat = Flat::default();
        if let Some(w) = self.windows.get(win) {
            let mut path = Vec::new();
            flatten_node(&w.layout, rect, &mut path, &mut flat);
        }
        flat
    }

    /// Drag splitter `handle` (from [`Flat::handles`]) by `delta` logical pixels.
    pub fn drag_handle(&mut self, win: usize, handle: &Handle, delta: f32) {
        let Some(w) = self.windows.get_mut(win) else {
            return;
        };
        let mut node = &mut w.layout;
        for &i in &handle.path {
            match node {
                Node::Split { children, .. } => node = &mut children[i],
                Node::Tabs { .. } => return,
            }
        }
        if let Node::Split { ratios, .. } = node {
            let i = handle.index;
            if i + 1 >= ratios.len() || handle.extent <= 0.0 {
                return;
            }
            let d = delta / handle.extent;
            let min = 0.08;
            let a = (ratios[i] + d).clamp(min, ratios[i] + ratios[i + 1] - min);
            let total = ratios[i] + ratios[i + 1];
            ratios[i] = a;
            ratios[i + 1] = total - a;
        }
    }
}

/// An axis-aligned rectangle in logical pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    /// Left.
    pub x: f32,
    /// Top.
    pub y: f32,
    /// Width.
    pub w: f32,
    /// Height.
    pub h: f32,
}

/// A tab stack placed at an absolute rect.
#[derive(Clone, Debug, PartialEq)]
pub struct StackRect {
    /// Stack index in depth-first order.
    pub stack: usize,
    /// Rect.
    pub rect: Rect,
    /// Panel ids.
    pub tabs: Vec<String>,
    /// Active tab index.
    pub active: usize,
}

/// A draggable splitter between two children of a split.
#[derive(Clone, Debug, PartialEq)]
pub struct Handle {
    /// Path of child indices from the root to the split.
    pub path: Vec<usize>,
    /// Index of the child before the handle.
    pub index: usize,
    /// Split direction.
    pub dir: Dir,
    /// Rect of the handle.
    pub rect: Rect,
    /// Length of the split along its axis.
    pub extent: f32,
}

/// Flattened layout.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Flat {
    /// Tab stacks.
    pub stacks: Vec<StackRect>,
    /// Splitter handles.
    pub handles: Vec<Handle>,
}

/// Splitter thickness in logical pixels.
pub const HANDLE: f32 = 6.0;

fn flatten_node(node: &Node, r: Rect, path: &mut Vec<usize>, out: &mut Flat) {
    match node {
        Node::Tabs { tabs, active } => out.stacks.push(StackRect {
            stack: out.stacks.len(),
            rect: r,
            tabs: tabs.clone(),
            active: *active,
        }),
        Node::Split {
            split,
            ratios,
            children,
        } => {
            let n = children.len();
            let extent = match split {
                Dir::Horizontal => r.w,
                Dir::Vertical => r.h,
            };
            let usable = (extent - HANDLE * (n.saturating_sub(1)) as f32).max(0.0);
            let mut pos = 0.0;
            for (i, c) in children.iter().enumerate() {
                let len = usable * ratios.get(i).copied().unwrap_or(1.0 / n as f32);
                let cr = match split {
                    Dir::Horizontal => Rect {
                        x: r.x + pos,
                        y: r.y,
                        w: len,
                        h: r.h,
                    },
                    Dir::Vertical => Rect {
                        x: r.x,
                        y: r.y + pos,
                        w: r.w,
                        h: len,
                    },
                };
                path.push(i);
                flatten_node(c, cr, path, out);
                path.pop();
                pos += len;
                if i + 1 < n {
                    let hr = match split {
                        Dir::Horizontal => Rect {
                            x: r.x + pos,
                            y: r.y,
                            w: HANDLE,
                            h: r.h,
                        },
                        Dir::Vertical => Rect {
                            x: r.x,
                            y: r.y + pos,
                            w: r.w,
                            h: HANDLE,
                        },
                    };
                    out.handles.push(Handle {
                        path: path.clone(),
                        index: i,
                        dir: *split,
                        rect: hr,
                        extent: usable,
                    });
                    pos += HANDLE;
                }
            }
        }
    }
}

fn contains(n: &Node, id: &str) -> bool {
    match n {
        Node::Tabs { tabs, .. } => tabs.iter().any(|t| t == id),
        Node::Split { children, .. } => children.iter().any(|c| contains(c, id)),
    }
}

fn collect_visible(n: &Node, out: &mut Vec<String>) {
    match n {
        Node::Tabs { tabs, active } => {
            if let Some(t) = tabs.get(*active) {
                out.push(t.clone());
            }
        }
        Node::Split { children, .. } => children.iter().for_each(|c| collect_visible(c, out)),
    }
}

fn collect_all(n: &Node, out: &mut Vec<String>) {
    match n {
        Node::Tabs { tabs, .. } => out.extend(tabs.iter().cloned()),
        Node::Split { children, .. } => children.iter().for_each(|c| collect_all(c, out)),
    }
}

fn count_stacks(n: &Node) -> usize {
    match n {
        Node::Tabs { .. } => 1,
        Node::Split { children, .. } => children.iter().map(count_stacks).sum(),
    }
}

fn stack_of(n: &Node, id: &str) -> Option<usize> {
    fn go(n: &Node, id: &str, idx: &mut usize) -> Option<usize> {
        match n {
            Node::Tabs { tabs, .. } => {
                let me = *idx;
                *idx += 1;
                tabs.iter().any(|t| t == id).then_some(me)
            }
            Node::Split { children, .. } => children.iter().find_map(|c| go(c, id, idx)),
        }
    }
    go(n, id, &mut 0)
}

fn nth_stack(n: &mut Node, target: usize) -> Option<&mut Node> {
    fn go<'a>(n: &'a mut Node, target: usize, idx: &mut usize) -> Option<&'a mut Node> {
        match n {
            Node::Tabs { .. } => {
                let me = *idx;
                *idx += 1;
                (me == target).then_some(n)
            }
            Node::Split { children, .. } => {
                for c in children.iter_mut() {
                    if let Some(found) = go(c, target, idx) {
                        return Some(found);
                    }
                }
                None
            }
        }
    }
    go(n, target, &mut 0)
}

fn remove(n: &mut Node, id: &str) {
    match n {
        Node::Tabs { tabs, active } => {
            if let Some(pos) = tabs.iter().position(|t| t == id) {
                tabs.remove(pos);
                if *active >= pos && *active > 0 {
                    *active -= 1;
                }
            }
        }
        Node::Split { children, .. } => children.iter_mut().for_each(|c| remove(c, id)),
    }
}

fn is_empty(n: &Node) -> bool {
    match n {
        Node::Tabs { tabs, .. } => tabs.is_empty(),
        Node::Split { children, .. } => children.iter().all(is_empty),
    }
}

/// Remove empty children and collapse single-child splits, renormalising ratios.
fn prune(n: &mut Node) {
    if let Node::Split {
        ratios, children, ..
    } = n
    {
        children.iter_mut().for_each(prune);
        let mut keep_r = Vec::new();
        let mut keep_c = Vec::new();
        for (c, r) in children.drain(..).zip(ratios.drain(..)) {
            if !is_empty(&c) {
                keep_c.push(c);
                keep_r.push(r);
            }
        }
        let sum: f32 = keep_r.iter().sum();
        if sum > 0.0 {
            keep_r.iter_mut().for_each(|r| *r /= sum);
        }
        if keep_c.len() == 1 {
            *n = keep_c.remove(0);
        } else if keep_c.is_empty() {
            *n = Node::Tabs {
                tabs: Vec::new(),
                active: 0,
            };
        } else {
            *ratios = keep_r;
            *children = keep_c;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_round_trip() {
        for w in [Workspace::edit(), Workspace::minimal()] {
            let s = w.to_toml();
            assert_eq!(Workspace::from_toml(&s).unwrap(), w, "{s}");
        }
    }

    #[test]
    fn close_open_float_dock() {
        let mut w = Workspace::edit();
        assert_eq!(w.stack_count(0), 4);
        w.close("rbs.live-controls");
        assert!(!w.is_open("rbs.live-controls"));
        assert_eq!(w.stack_count(0), 3);
        w.open("rbs.live-controls");
        assert!(w.is_open("rbs.live-controls"));
        let f = w.float("rbs.program").unwrap();
        assert_eq!(w.windows.len(), 2);
        assert_eq!(w.windows[f].kind, WindowKind::Floating);
        w.dock(f);
        assert_eq!(w.windows.len(), 1);
        assert!(w.is_open("rbs.program"));
    }

    #[test]
    fn move_and_flatten() {
        let mut w = Workspace::minimal();
        w.move_to_next_stack("rbs.program");
        // Program joined the second stack; the first became empty and was pruned.
        assert_eq!(w.stack_count(0), 1);
        let flat = w.flatten(
            0,
            Rect {
                x: 0.0,
                y: 0.0,
                w: 800.0,
                h: 600.0,
            },
        );
        assert_eq!(flat.stacks.len(), 1);
        assert!(flat.handles.is_empty());
    }

    #[test]
    fn drag_handle_keeps_sum() {
        let mut w = Workspace::minimal();
        let flat = w.flatten(
            0,
            Rect {
                x: 0.0,
                y: 0.0,
                w: 800.0,
                h: 606.0,
            },
        );
        let h = flat.handles[0].clone();
        w.drag_handle(0, &h, -60.0);
        if let Node::Split { ratios, .. } = &w.windows[0].layout {
            assert!((ratios[0] - 0.7).abs() < 1e-3, "{ratios:?}");
            assert!((ratios.iter().sum::<f32>() - 1.0).abs() < 1e-5);
        } else {
            panic!("expected split");
        }
    }
}

/// Fake audio analysis data for the custom drawing test: `bands` spectrum
/// values in 0..1 at time `t`, written into `out` without allocating.
pub fn spectrum(t: f32, out: &mut [f32]) {
    let n = out.len().max(1) as f32;
    for (i, v) in out.iter_mut().enumerate() {
        let x = i as f32 / n;
        let s = 0.55 * (1.0 - x)
            + 0.25 * (t * 3.1 + x * 17.0).sin().abs()
            + 0.2 * (t * 7.3 + x * 41.0).sin().abs();
        *v = s.clamp(0.0, 1.0);
    }
}

/// SVG path commands for a spectrum polyline in a `w` by `h` box, reusing `buf`.
pub fn spectrum_path(values: &[f32], w: f32, h: f32, buf: &mut String) {
    use std::fmt::Write;
    buf.clear();
    let n = values.len().saturating_sub(1).max(1) as f32;
    for (i, v) in values.iter().enumerate() {
        let x = w * i as f32 / n;
        let y = h * (1.0 - v);
        let _ = write!(buf, "{}{x:.1} {y:.1} ", if i == 0 { "M " } else { "L " });
    }
}

/// Prints the animation tick rate every two seconds when `RBS_PROTO_FPS` is set.
/// Used by the measurement script to normalise CPU cost per frame.
pub struct FpsLog {
    enabled: bool,
    since: std::time::Instant,
    ticks: u32,
}

impl Default for FpsLog {
    fn default() -> Self {
        FpsLog {
            enabled: std::env::var_os("RBS_PROTO_FPS").is_some(),
            since: std::time::Instant::now(),
            ticks: 0,
        }
    }
}

impl FpsLog {
    /// Count one tick.
    pub fn tick(&mut self) {
        if !self.enabled {
            return;
        }
        self.ticks += 1;
        let e = self.since.elapsed().as_secs_f32();
        if e >= 2.0 {
            eprintln!("fps {:.1}", self.ticks as f32 / e);
            self.ticks = 0;
            self.since = std::time::Instant::now();
        }
    }
}

/// Prototype settings read from the environment, shared by all prototypes.
pub mod env {
    /// Engine output size from `RBS_PROTO_SIZE` (for example `1920x1080`), default 1280x720.
    pub fn engine_size() -> (u32, u32) {
        std::env::var("RBS_PROTO_SIZE")
            .ok()
            .and_then(|s| {
                let (w, h) = s.split_once('x')?;
                Some((w.parse().ok()?, h.parse().ok()?))
            })
            .unwrap_or((1280, 720))
    }

    /// Spectrum band count from `RBS_PROTO_BANDS`, default 64. Large values are the dense-path stress case.
    pub fn bands() -> usize {
        std::env::var("RBS_PROTO_BANDS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(64)
            .max(2)
    }
}

/// Self-measurement of presented frames and process CPU, enabled with
/// `RBS_PROTO_MEASURE=<seconds>`. After a 3 s warm-up it records the time of
/// every presented frame for the given seconds, prints one summary line and
/// exits the process. Works on Linux, macOS and Windows (CPU on Unix only).
pub struct Measure {
    secs: f64,
    start: std::time::Instant,
    frames: Vec<std::time::Instant>,
    cpu0: Option<f64>,
    label: String,
}

fn cpu_seconds() -> Option<f64> {
    #[cfg(unix)]
    {
        // SAFETY: getrusage writes into the zeroed struct we own.
        let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
        let r = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
        if r != 0 {
            return None;
        }
        let tv = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
        Some(tv(ru.ru_utime) + tv(ru.ru_stime))
    }
    #[cfg(not(unix))]
    {
        None
    }
}

fn max_rss_mib() -> Option<f64> {
    #[cfg(unix)]
    {
        // SAFETY: as above.
        let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
        if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) } != 0 {
            return None;
        }
        // ru_maxrss is bytes on macOS and KiB on Linux.
        let div = if cfg!(target_os = "macos") {
            1024.0 * 1024.0
        } else {
            1024.0
        };
        Some(ru.ru_maxrss as f64 / div)
    }
    #[cfg(not(unix))]
    {
        None
    }
}

impl Measure {
    /// Create from the environment; `None` when measuring is off.
    pub fn from_env(label: &str) -> Option<Self> {
        let secs: f64 = std::env::var("RBS_PROTO_MEASURE").ok()?.parse().ok()?;
        Some(Measure {
            secs,
            start: std::time::Instant::now(),
            frames: Vec::new(),
            cpu0: None,
            label: label.into(),
        })
    }

    /// Record one presented frame; exits the process when the window is over.
    pub fn presented(&mut self) {
        let now = std::time::Instant::now();
        let t = now.duration_since(self.start).as_secs_f64();
        if t < 3.0 {
            return;
        }
        if self.cpu0.is_none() {
            self.cpu0 = cpu_seconds();
            self.frames.clear();
        }
        self.frames.push(now);
        if t >= 3.0 + self.secs {
            self.finish();
        }
    }

    fn finish(&self) {
        let n = self.frames.len();
        let span = match (self.frames.first(), self.frames.last()) {
            (Some(a), Some(b)) => b.duration_since(*a).as_secs_f64(),
            _ => 0.0,
        };
        let mut iv: Vec<f64> = self
            .frames
            .windows(2)
            .map(|w| w[1].duration_since(w[0]).as_secs_f64() * 1000.0)
            .collect();
        iv.sort_by(|a, b| a.total_cmp(b));
        let pct = |p: f64| {
            iv.get(((iv.len() as f64 - 1.0) * p).round() as usize)
                .copied()
                .unwrap_or(0.0)
        };
        // Late: an interval longer than 1.5 refresh periods at 60 Hz.
        let late = iv.iter().filter(|v| **v > 25.0).count();
        let cpu = match (self.cpu0, cpu_seconds()) {
            (Some(a), Some(b)) if span > 0.0 => format!("{:.1}", 100.0 * (b - a) / span),
            _ => "n/a".into(),
        };
        let rss = max_rss_mib()
            .map(|r| format!("{r:.0}"))
            .unwrap_or_else(|| "n/a".into());
        println!(
            "MEASURE {} frames={} fps={:.1} late={} late_pct={:.2} p50_ms={:.1} p99_ms={:.1} max_ms={:.1} cpu_pct={} max_rss_mib={}",
            self.label,
            n,
            if span > 0.0 {
                (n.saturating_sub(1)) as f64 / span
            } else {
                0.0
            },
            late,
            if iv.is_empty() {
                0.0
            } else {
                100.0 * late as f64 / iv.len() as f64
            },
            pct(0.5),
            pct(0.99),
            pct(1.0),
            cpu,
            rss
        );
        std::process::exit(0);
    }
}
