//! The choices of the Source settings window and the settings patch they make
//! (ADR-0077 decision 5; colour choice ADR-0175).

/// The frame rates a screen, a window or a camera mode is offered, lowest first.
const STANDARD_RATES: [u32; 12] = [5, 10, 15, 20, 24, 25, 30, 48, 50, 60, 90, 120];

/// What the choices of the Source settings window stand for.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct SettingsChoices {
    /// Camera modes: the size of each, in the order listed.
    pub(super) modes: Vec<(u32, u32)>,
    /// The rates of each mode (one list for a screen or window).
    pub(super) rates: Vec<Vec<u32>>,
    /// The kind has a cursor choice.
    pub(super) has_cursor: bool,
    /// The camera's adapter honours a colour choice (ADR-0175).
    pub(super) has_color: bool,
}

impl SettingsChoices {
    /// The settings patch for the chosen mode and rate (`-1`: nothing chosen, the stored
    /// value stays) and the cursor.
    pub(super) fn patch(
        &self,
        mode: i32,
        rate: i32,
        cursor: bool,
        color: (i32, i32),
    ) -> std::collections::BTreeMap<String, serde_json::Value> {
        let mut patch = std::collections::BTreeMap::new();
        let mode = usize::try_from(mode).ok();
        if let Some((w, h)) = mode.and_then(|m| self.modes.get(m)) {
            patch.insert("width".to_owned(), serde_json::json!(w));
            patch.insert("height".to_owned(), serde_json::json!(h));
        }
        let list = if self.modes.is_empty() {
            self.rates.first()
        } else {
            mode.and_then(|m| self.rates.get(m))
        };
        if let Some(fps) = usize::try_from(rate)
            .ok()
            .and_then(|r| list.and_then(|l| l.get(r)))
        {
            patch.insert("fps".to_owned(), serde_json::json!(fps));
        }
        if self.has_cursor {
            patch.insert("cursor".to_owned(), serde_json::json!(cursor));
        }
        if self.has_color {
            // Automatic removes the key (ADR-0175).
            let matrix = match color.0 {
                1 => serde_json::json!("bt709"),
                2 => serde_json::json!("bt601"),
                _ => serde_json::Value::Null,
            };
            let range = match color.1 {
                1 => serde_json::json!("limited"),
                2 => serde_json::json!("full"),
                _ => serde_json::Value::Null,
            };
            patch.insert("color_matrix".to_owned(), matrix);
            patch.insert("color_range".to_owned(), range);
        }
        patch
    }
}

/// The rates of a mode that runs from `min` to `max` frames per second, plus the stored
/// one so that it can be shown.
pub(super) fn rates_between(min: u32, max: u32, stored: Option<u32>) -> Vec<u32> {
    let mut rates: Vec<u32> = STANDARD_RATES
        .into_iter()
        .filter(|r| (min..=max).contains(r))
        .collect();
    if rates.is_empty() {
        rates.push(max.max(1));
    }
    if let Some(s) = stored.filter(|s| !rates.contains(s)) {
        rates.push(s);
        rates.sort_unstable();
    }
    rates
}

/// A camera's modes as the Source settings window lists them: each picture size once,
/// largest first (ties: the wider one), with every rate any of its modes runs at. A
/// camera reports one mode per size, pixel format and rate range, and the port picks
/// the pixel format itself, so listing those separately only repeats the same size.
pub(super) fn camera_sizes(modes: &[rbs_sources::SourceMode]) -> Vec<((u32, u32), Vec<u32>)> {
    let mut sizes: Vec<((u32, u32), Vec<u32>)> = Vec::new();
    for m in modes {
        let rates = rates_between(m.min_fps, m.max_fps, None);
        match sizes.iter_mut().find(|(s, _)| *s == (m.width, m.height)) {
            Some((_, have)) => {
                have.extend(rates);
                have.sort_unstable();
                have.dedup();
            }
            None => sizes.push(((m.width, m.height), rates)),
        }
    }
    sizes.sort_by(|((aw, ah), _), ((bw, bh), _)| {
        (u64::from(*bw) * u64::from(*bh), *bw).cmp(&(u64::from(*aw) * u64::from(*ah), *aw))
    });
    sizes
}
