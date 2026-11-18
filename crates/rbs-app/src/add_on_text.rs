//! The words of Settings › Add-ons (ADR-0145 decision 9): one row per catalog entry,
//! from what `addon.list` and `addon.changed` report. Pure.

use rbs_api::types::{AddOnFailReason, AddOnInfo, AddOnStatus};

/// What a row's button does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// No button.
    None,
    /// **Download**.
    Download,
    /// **Cancel** a running download.
    Cancel,
    /// **Remove** every version.
    Remove,
    /// **Update** to the catalog's version.
    Update,
    /// **Retry** after a failure.
    Retry,
}

impl Action {
    /// The number the page uses (`AddOnRowData.action`).
    pub fn code(self) -> i32 {
        match self {
            Action::None => 0,
            Action::Download => 1,
            Action::Cancel => 2,
            Action::Remove => 3,
            Action::Update => 4,
            Action::Retry => 5,
        }
    }

    /// The action of a page number.
    pub fn from_code(code: i32) -> Self {
        match code {
            1 => Action::Download,
            2 => Action::Cancel,
            3 => Action::Remove,
            4 => Action::Update,
            5 => Action::Retry,
            _ => Action::None,
        }
    }

    /// The button's text.
    pub fn text(self) -> &'static str {
        match self {
            Action::None => "",
            Action::Download => "Download",
            Action::Cancel => "Cancel",
            Action::Remove => "Remove",
            Action::Update => "Update",
            Action::Retry => "Retry",
        }
    }
}

/// One row of the page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// The add-on's id.
    pub id: String,
    /// Its name.
    pub label: String,
    /// The line under the name.
    pub help: String,
    /// The line is a problem (a failed install).
    pub problem: bool,
    /// What the control column shows beside the button; empty for none.
    pub value: String,
    /// The button.
    pub action: Action,
}

/// A size in bytes as the page shows it: "175 MB", "1.6 GB" (decimal units).
pub fn size(bytes: u64) -> String {
    const MB: u64 = 1_000_000;
    const GB: u64 = 1_000_000_000;
    if bytes >= GB {
        let tenths = (bytes + GB / 20) / (GB / 10);
        format!("{}.{} GB", tenths / 10, tenths % 10)
    } else {
        format!("{} MB", (bytes + MB / 2) / MB)
    }
}

/// Bytes as whole megabytes, for progress ("42").
fn megabytes(bytes: u64) -> u64 {
    bytes / 1_000_000
}

fn sizes(info: &AddOnInfo) -> String {
    match (info.size, info.unpacked_size) {
        (Some(d), Some(u)) => format!("{} download, {} on disk.", size(d), size(u)),
        _ => String::new(),
    }
}

fn reason(r: AddOnFailReason) -> &'static str {
    match r {
        AddOnFailReason::Network => "The download failed. Check your connection.",
        AddOnFailReason::HostNotAllowed => "The download went to a server RBS does not trust.",
        AddOnFailReason::TooLarge => "The download was larger than expected.",
        AddOnFailReason::HashMismatch => "The download was damaged or changed on the way.",
        AddOnFailReason::UnsafeArchive => "The download has files RBS will not unpack.",
        AddOnFailReason::NoSpace => "There is not enough free space.",
        AddOnFailReason::Io => "The files could not be written.",
        AddOnFailReason::Other => "The download failed.",
    }
}

/// The row of `info`.
pub fn row(info: &AddOnInfo) -> Row {
    let summary = |extra: &str| {
        if extra.is_empty() {
            info.summary.clone()
        } else {
            format!("{} {extra}", info.summary)
        }
    };
    let mut r = Row {
        id: info.id.clone(),
        label: info.name.clone(),
        help: summary(&sizes(info)),
        problem: false,
        value: String::new(),
        action: Action::None,
    };
    if !info.available {
        r.help = info.summary.clone();
        r.value = "Not available for this computer".into();
        return r;
    }
    match &info.status {
        AddOnStatus::NotInstalled | AddOnStatus::Other => r.action = Action::Download,
        AddOnStatus::Downloading { bytes, total } => {
            r.help = format!(
                "Downloading: {} of {} MB.",
                megabytes(*bytes),
                megabytes(*total)
            );
            r.action = Action::Cancel;
        }
        AddOnStatus::Verifying => {
            r.help = "Checking the download…".into();
            r.action = Action::Cancel;
        }
        AddOnStatus::Installing => {
            r.help = "Installing…".into();
            r.action = Action::Cancel;
        }
        AddOnStatus::Installed { version } => {
            r.help = info.summary.clone();
            r.value = format!("Installed {version}");
            r.action = Action::Remove;
        }
        AddOnStatus::UpdateAvailable { installed, .. } => {
            r.value = format!("Installed {installed}");
            r.action = Action::Update;
        }
        AddOnStatus::Failed { reason: why } => {
            r.help = reason(*why).into();
            r.problem = true;
            r.action = Action::Retry;
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine(status: AddOnStatus) -> AddOnInfo {
        AddOnInfo {
            id: "org.rbs.browser-engine".into(),
            kind: "runtime".into(),
            name: "Browser engine".into(),
            summary: "Lets browser sources show web pages.".into(),
            version: "156.0.30".into(),
            size: Some(175_176_258),
            unpacked_size: Some(1_597_510_500),
            available: true,
            status,
        }
    }

    #[test]
    fn sizes_read_in_decimal_units() {
        assert_eq!(size(175_176_258), "175 MB");
        assert_eq!(size(444_764_149), "445 MB");
        assert_eq!(size(1_597_510_500), "1.6 GB");
        assert_eq!(size(0), "0 MB");
    }

    #[test]
    fn each_status_has_its_line_and_one_command() {
        let r = row(&engine(AddOnStatus::NotInstalled));
        assert_eq!(
            r.help,
            "Lets browser sources show web pages. 175 MB download, 1.6 GB on disk."
        );
        assert_eq!((r.action, r.value.as_str()), (Action::Download, ""));
        let r = row(&engine(AddOnStatus::Downloading {
            bytes: 42_000_000,
            total: 175_176_258,
        }));
        assert_eq!(r.help, "Downloading: 42 of 175 MB.");
        assert_eq!(r.action, Action::Cancel);
        let r = row(&engine(AddOnStatus::Installed {
            version: "156.0.30".into(),
        }));
        assert_eq!(
            (r.value.as_str(), r.action),
            ("Installed 156.0.30", Action::Remove)
        );
        let r = row(&engine(AddOnStatus::UpdateAvailable {
            installed: "156.0.29".into(),
            available: "156.0.30".into(),
        }));
        assert_eq!(
            (r.value.as_str(), r.action),
            ("Installed 156.0.29", Action::Update)
        );
        let r = row(&engine(AddOnStatus::Failed {
            reason: AddOnFailReason::Network,
        }));
        assert!(r.problem);
        assert_eq!(r.action, Action::Retry);
        let mut elsewhere = engine(AddOnStatus::NotInstalled);
        elsewhere.available = false;
        let r = row(&elsewhere);
        assert_eq!(
            (r.value.as_str(), r.action),
            ("Not available for this computer", Action::None)
        );
    }

    #[test]
    fn every_line_fits_a_settings_row() {
        // The help line limit of `cargo xtask check-ui` (90 characters).
        for why in [
            AddOnFailReason::Network,
            AddOnFailReason::HostNotAllowed,
            AddOnFailReason::TooLarge,
            AddOnFailReason::HashMismatch,
            AddOnFailReason::UnsafeArchive,
            AddOnFailReason::NoSpace,
            AddOnFailReason::Io,
            AddOnFailReason::Other,
        ] {
            assert!(reason(why).chars().count() <= 90);
        }
        assert!(row(&engine(AddOnStatus::NotInstalled)).help.chars().count() <= 90);
        for code in 0..=6 {
            let a = Action::from_code(code);
            assert!(code > 5 || a.code() == code);
        }
    }
}
