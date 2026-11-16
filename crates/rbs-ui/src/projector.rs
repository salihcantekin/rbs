//! Projectors (ADR-0125): what a workspace's projector content string means, and the
//! sizes a sharing app receives.
//!
//! Spec: `docs/specs/ui-design-system.md`, "Projector window".
//!
//! A projector's content is a string the shell interprets (ADR-0017):
//! `program` for the main canvas's program, `program?canvas=vertical` for the vertical
//! canvas's, with `size=exact` for Exact pixels (ADR-0125 decision 10). Full screen is
//! the window's `placement.fullscreen`, not a parameter. Parameters this build does not
//! know are kept as written, so a newer build's file survives a save.

use std::fmt;

/// Whose program a projector shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ProjectorCanvas {
    /// The main canvas.
    Main,
    /// The vertical canvas (ADR-0093).
    Vertical,
}

/// How a projector's window is sized (ADR-0125 decision 8). Full screen is kept in the
/// window's placement.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProjectorSize {
    /// Any size the user gives it.
    #[default]
    Window,
    /// Its physical pixels equal the canvas, or a whole fraction of it that fits.
    Exact,
}

/// A projector's content string, parsed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectorContent {
    /// Whose program.
    pub canvas: ProjectorCanvas,
    /// Its size.
    pub size: ProjectorSize,
    /// Parameters this build does not know, kept in their order.
    pub other: Vec<(String, String)>,
}

impl ProjectorContent {
    /// The program of `canvas`, sized as a window.
    pub fn program(canvas: ProjectorCanvas) -> Self {
        Self {
            canvas,
            size: ProjectorSize::Window,
            other: Vec::new(),
        }
    }

    /// Parses a content string. `None` for content this build does not open (a scene
    /// or source projector, ADR-0125 decision 1); such a window stays in the file.
    /// A `size` value this build does not know reads as Window and is kept.
    pub fn parse(content: &str) -> Option<Self> {
        let (head, query) = content.split_once('?').unwrap_or((content, ""));
        if head != "program" {
            return None;
        }
        let mut parsed = Self::program(ProjectorCanvas::Main);
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            match (key, value) {
                ("canvas", "vertical") => parsed.canvas = ProjectorCanvas::Vertical,
                // Another canvas id: not one this build can show.
                ("canvas", _) => return None,
                ("size", "exact") => parsed.size = ProjectorSize::Exact,
                ("size", "window") => parsed.size = ProjectorSize::Window,
                _ => parsed.other.push((key.to_owned(), value.to_owned())),
            }
        }
        Some(parsed)
    }

    /// The same projector with another size.
    pub fn with_size(mut self, size: ProjectorSize) -> Self {
        self.size = size;
        self
    }

    /// The string that names its canvas only (`program`, `program?canvas=vertical`):
    /// two projectors of one canvas are the same projector (ADR-0125 decision 9).
    pub fn key(&self) -> &'static str {
        match self.canvas {
            ProjectorCanvas::Main => "program",
            ProjectorCanvas::Vertical => "program?canvas=vertical",
        }
    }
}

impl fmt::Display for ProjectorContent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("program")?;
        let mut sep = '?';
        let mut put = |f: &mut fmt::Formatter<'_>, k: &str, v: &str| {
            let r = write!(f, "{sep}{k}={v}");
            sep = '&';
            r
        };
        if self.canvas == ProjectorCanvas::Vertical {
            put(f, "canvas", "vertical")?;
        }
        if self.size == ProjectorSize::Exact {
            put(f, "size", "exact")?;
        }
        for (k, v) in &self.other {
            put(f, k, v)?;
        }
        Ok(())
    }
}

/// The content size in physical pixels for Exact pixels (ADR-0125 decision 8):
/// `canvas / k` with `k` the smallest whole number for which it fits `visible` (the
/// display's visible area in physical pixels), and that `k`. A side never goes below
/// one pixel; a `visible` of zero gives the largest `k` that keeps one pixel.
pub fn exact_size(canvas: [u32; 2], visible: [u32; 2]) -> ([u32; 2], u32) {
    let need = |c: u32, v: u32| -> u32 {
        if v == 0 {
            return c.max(1);
        }
        c.div_ceil(v).max(1)
    };
    let k = need(canvas[0], visible[0])
        .max(need(canvas[1], visible[1]))
        .max(1);
    ([(canvas[0] / k).max(1), (canvas[1] / k).max(1)], k)
}

/// The picture a sharing app receives from a window whose content is `window`
/// physical pixels: the canvas fitted inside it, letterboxed (ADR-0125 decision 8).
pub fn shared_size(window: [u32; 2], canvas: [u32; 2]) -> [u32; 2] {
    if canvas[0] == 0 || canvas[1] == 0 || window[0] == 0 || window[1] == 0 {
        return [0, 0];
    }
    let (w, h) = (u64::from(window[0]), u64::from(window[1]));
    let (cw, ch) = (u64::from(canvas[0]), u64::from(canvas[1]));
    // The side that limits the fit keeps the window's size; the other follows the
    // canvas's aspect, rounded down.
    if w * ch <= h * cw {
        let height = (w * ch / cw) as u32;
        [window[0], height.max(1)]
    } else {
        let width = (h * cw / ch) as u32;
        [width.max(1), window[1]]
    }
}

/// The first line of a projector's menu: "Shared at 1920 × 1080", with "(half the
/// canvas)" for Exact pixels at `k = 2` and "smaller than the canvas" when a window
/// shares less than the canvas.
pub fn shared_text(shared: [u32; 2], canvas: [u32; 2], exact_k: Option<u32>) -> String {
    let size = format!("Shared at {} × {}", shared[0], shared[1]);
    match exact_k {
        Some(1) => size,
        Some(2) => format!("{size} (half the canvas)"),
        Some(3) => format!("{size} (a third of the canvas)"),
        Some(k) => format!("{size} (1/{k} of the canvas)"),
        None if shared[0] < canvas[0] || shared[1] < canvas[1] => {
            format!("{size}, smaller than the canvas")
        }
        None => size,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    #[test]
    fn content_strings_round_trip_and_keep_what_they_do_not_know() {
        for s in [
            "program",
            "program?canvas=vertical",
            "program?size=exact",
            "program?canvas=vertical&size=exact",
            "program?size=exact&future=1",
        ] {
            assert_eq!(ProjectorContent::parse(s).unwrap().to_string(), s, "{s}");
        }
        let c = ProjectorContent::parse("program?canvas=vertical&size=exact").unwrap();
        assert_eq!(c.canvas, ProjectorCanvas::Vertical);
        assert_eq!(c.size, ProjectorSize::Exact);
        assert_eq!(c.key(), "program?canvas=vertical");
        // `size=window` is the default and is not written.
        assert_eq!(
            ProjectorContent::parse("program?size=window")
                .unwrap()
                .to_string(),
            "program"
        );
        // An unknown size reads as Window and is kept.
        let odd = ProjectorContent::parse("program?size=huge").unwrap();
        assert_eq!(odd.size, ProjectorSize::Window);
        assert_eq!(odd.to_string(), "program?size=huge");
        // Scene and source projectors, and other canvases, are not opened.
        assert!(ProjectorContent::parse("scene?id=3").is_none());
        assert!(ProjectorContent::parse("program?canvas=7").is_none());
    }

    #[test]
    fn exact_pixels_takes_the_largest_whole_fraction_that_fits() {
        // Fits whole on a Retina laptop (physical pixels).
        assert_eq!(exact_size([1920, 1080], [3024, 1890]), ([1920, 1080], 1));
        // A 4K canvas on that display: half.
        assert_eq!(exact_size([3840, 2160], [3024, 1890]), ([1920, 1080], 2));
        // A vertical canvas on a 1080p display at scale 1: half its height fits.
        assert_eq!(exact_size([1080, 1920], [1920, 1040]), ([540, 960], 2));
        // At scale 1.5 the same rule in physical pixels.
        assert_eq!(exact_size([1920, 1080], [2880, 1560]), ([1920, 1080], 1));
        // Scale 1.25 on a small display: a third.
        assert_eq!(exact_size([3840, 2160], [1600, 860]), ([1280, 720], 3));
        // No visible area: never a zero side.
        let (size, _) = exact_size([1920, 1080], [0, 0]);
        assert!(size[0] >= 1 && size[1] >= 1);
    }

    #[test]
    fn the_shared_size_is_the_canvas_fitted_in_the_window() {
        assert_eq!(shared_size([1920, 1080], [1920, 1080]), [1920, 1080]);
        // Wider window: pillarboxed.
        assert_eq!(shared_size([2000, 1080], [1920, 1080]), [1920, 1080]);
        // Taller window: letterboxed.
        assert_eq!(shared_size([1280, 1000], [1920, 1080]), [1280, 720]);
        assert_eq!(shared_size([400, 400], [1080, 1920]), [225, 400]);
        assert_eq!(shared_size([0, 400], [1920, 1080]), [0, 0]);
    }

    #[test]
    fn the_shared_text_says_when_the_share_is_smaller() {
        assert_eq!(
            shared_text([1920, 1080], [1920, 1080], None),
            "Shared at 1920 × 1080"
        );
        assert_eq!(
            shared_text([1280, 720], [1920, 1080], None),
            "Shared at 1280 × 720, smaller than the canvas"
        );
        assert_eq!(
            shared_text([1920, 1080], [3840, 2160], Some(2)),
            "Shared at 1920 × 1080 (half the canvas)"
        );
        assert_eq!(
            shared_text([1920, 1080], [1920, 1080], Some(1)),
            "Shared at 1920 × 1080"
        );
    }

    #[test]
    fn a_workspace_keeps_one_projector_per_canvas_and_what_it_cannot_open() {
        use crate::workspace::{Placement, Window, WindowContent, Workspace};

        let mut ws = crate::builtin::edit();
        // A scene projector from a newer build stays in the file.
        ws.windows.push(Window {
            content: WindowContent::Projector("scene?id=3".into()),
            placement: None,
        });
        let main = ProjectorContent::program(ProjectorCanvas::Main);
        ws.set_projector(&main, None);
        let full = Placement {
            connector: Some("Studio Display".into()),
            size: Some([5120, 2880]),
            fullscreen: true,
            ..Placement::default()
        };
        // The same canvas again changes that projector, it does not add one.
        ws.set_projector(
            &main.clone().with_size(ProjectorSize::Exact),
            Some(full.clone()),
        );
        let open = ws.projectors();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].1.size, ProjectorSize::Exact);
        assert_eq!(ws.windows[open[0].0].placement, Some(full));
        ws.validate().unwrap();
        // Saved and loaded again.
        let back = Workspace::from_toml(&ws.to_toml().unwrap()).unwrap();
        assert_eq!(back.projectors(), ws.projectors());
        assert!(ws.close_projector("program"));
        assert!(!ws.close_projector("program"));
        assert!(ws.projectors().is_empty());
        assert!(
            ws.windows
                .iter()
                .any(|w| w.content == WindowContent::Projector("scene?id=3".into()))
        );
    }
}
