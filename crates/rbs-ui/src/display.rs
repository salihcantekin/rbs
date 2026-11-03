//! Placing saved windows on the displays that are connected now.
//!
//! Spec: `docs/specs/ui-design-system.md`, "Workspace file": match by EDID
//! fingerprint, then connector with the same size, then any display with the
//! same size and scale; otherwise the window docks into the main window and a
//! projector is not opened. A rect outside the matched display is clamped to it.

use crate::workspace::Placement;

/// A display that is connected now, as the shell reports it.
#[derive(Clone, Debug, PartialEq)]
pub struct Display {
    /// EDID fingerprint, for example `edid:DEL-41A3-7H2K9`.
    pub fingerprint: String,
    /// Connector name, for example `DP-2`.
    pub connector: String,
    /// Size in physical pixels.
    pub size: [u32; 2],
    /// Scale factor.
    pub scale: f32,
}

impl Display {
    /// Size in logical pixels.
    pub fn logical_size(&self) -> [f32; 2] {
        let s = if self.scale > 0.0 { self.scale } else { 1.0 };
        [self.size[0] as f32 / s, self.size[1] as f32 / s]
    }
}

/// Which rule matched a saved placement to a display.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Match {
    /// Same EDID fingerprint.
    Fingerprint,
    /// Same connector and size.
    Connector,
    /// Same size and scale.
    SizeAndScale,
}

/// Where a saved window goes now.
#[derive(Clone, Debug, PartialEq)]
pub enum Resolved {
    /// On display `display` (an index into the slice given), at `rect`
    /// (`[x, y, w, h]`, logical pixels relative to that display; `None` when the
    /// file had no rect).
    OnDisplay {
        /// Index of the display.
        display: usize,
        /// Rule that matched.
        by: Match,
        /// Clamped rect.
        rect: Option<[f32; 4]>,
    },
    /// No display matched: dock floating windows into the main window, and do
    /// not open projectors.
    NoMatch,
}

fn same_scale(a: f32, b: f32) -> bool {
    (a - b).abs() < 0.01
}

/// Find the display for a saved placement.
pub fn resolve(placement: &Placement, displays: &[Display]) -> Resolved {
    let by_fingerprint = placement.display.as_ref().and_then(|f| {
        displays
            .iter()
            .position(|d| &d.fingerprint == f)
            .map(|i| (i, Match::Fingerprint))
    });
    let by_connector = || {
        let (c, size) = (placement.connector.as_ref()?, placement.size?);
        displays
            .iter()
            .position(|d| &d.connector == c && d.size == size)
            .map(|i| (i, Match::Connector))
    };
    let by_size = || {
        let (size, scale) = (placement.size?, placement.scale?);
        displays
            .iter()
            .position(|d| d.size == size && same_scale(d.scale, scale))
            .map(|i| (i, Match::SizeAndScale))
    };
    match by_fingerprint.or_else(by_connector).or_else(by_size) {
        Some((i, by)) => Resolved::OnDisplay {
            display: i,
            by,
            rect: placement.rect.map(|r| clamp(r, displays[i].logical_size())),
        },
        None => Resolved::NoMatch,
    }
}

/// Clamp `[x, y, w, h]` into a display of logical size `[w, h]`: shrink to fit, then move inside.
pub fn clamp(rect: [f32; 4], display: [f32; 2]) -> [f32; 4] {
    let w = rect[2].min(display[0]).max(0.0);
    let h = rect[3].min(display[1]).max(0.0);
    let x = rect[0].clamp(0.0, display[0] - w);
    let y = rect[1].clamp(0.0, display[1] - h);
    [x, y, w, h]
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    fn displays() -> Vec<Display> {
        vec![
            Display {
                fingerprint: "edid:AAA-1".into(),
                connector: "eDP-1".into(),
                size: [3024, 1964],
                scale: 2.0,
            },
            Display {
                fingerprint: "edid:BBB-2".into(),
                connector: "DP-2".into(),
                size: [2560, 1440],
                scale: 1.5,
            },
        ]
    }

    fn placement(
        display: Option<&str>,
        connector: Option<&str>,
        size: [u32; 2],
        scale: f32,
    ) -> Placement {
        Placement {
            display: display.map(Into::into),
            connector: connector.map(Into::into),
            size: Some(size),
            scale: Some(scale),
            rect: Some([100.0, 100.0, 800.0, 600.0]),
            fullscreen: false,
        }
    }

    #[test]
    fn matching_order() {
        let d = displays();
        let p = placement(Some("edid:BBB-2"), Some("eDP-1"), [3024, 1964], 2.0);
        assert!(matches!(
            resolve(&p, &d),
            Resolved::OnDisplay {
                display: 1,
                by: Match::Fingerprint,
                ..
            }
        ));
        let p = placement(Some("edid:GONE"), Some("DP-2"), [2560, 1440], 1.0);
        assert!(matches!(
            resolve(&p, &d),
            Resolved::OnDisplay {
                display: 1,
                by: Match::Connector,
                ..
            }
        ));
        // An exported placement has neither fingerprint nor connector.
        let p = placement(None, None, [3024, 1964], 2.0);
        assert!(matches!(
            resolve(&p, &d),
            Resolved::OnDisplay {
                display: 0,
                by: Match::SizeAndScale,
                ..
            }
        ));
        let p = placement(Some("edid:GONE"), Some("HDMI-1"), [1920, 1080], 1.0);
        assert_eq!(resolve(&p, &d), Resolved::NoMatch);
        assert_eq!(resolve(&Placement::default(), &d), Resolved::NoMatch);
    }

    #[test]
    fn rect_is_clamped_to_the_display() {
        let d = displays();
        let mut p = placement(Some("edid:BBB-2"), None, [2560, 1440], 1.5);
        p.rect = Some([1500.0, 900.0, 800.0, 2000.0]);
        let Resolved::OnDisplay { rect: Some(r), .. } = resolve(&p, &d) else {
            panic!()
        };
        // Logical size of display 1 is 1706.7 x 960.
        assert_eq!(r[3], 960.0);
        assert_eq!(r[1], 0.0);
        assert!((r[0] + r[2] - 2560.0 / 1.5).abs() < 0.01);
    }
}
