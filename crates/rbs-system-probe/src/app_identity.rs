//! Which app the OS gives privacy permissions to (ADR-0141 decisions 5 and 10).
//!
//! macOS gives a camera, microphone or screen recording grant to the *responsible*
//! app: for a bundled RBS started from Finder or `open`, RBS itself; for a plain
//! executable, or a bundle's executable started from a terminal, the app that started
//! it. The public API cannot name the responsible process, so this walks the parent
//! processes up to the first one inside a `.app`. A prompt is safe only when RBS is
//! its own responsible app and its `Info.plist` declares the usage key; otherwise
//! macOS may end the process at the request.
//!
//! The decisions are pure functions over paths and plist text; [`ancestors`] and
//! [`current`] read the OS. Nothing here runs unless called.

use std::path::{Path, PathBuf};

/// How far [`ancestors`] walks up.
pub const MAX_ANCESTORS: usize = 16;

/// The bundle a path is inside: the outermost-first `.app` directory on its way from
/// the root, cut at the first one (`/Applications/RBS.app/Contents/MacOS/rbs` gives
/// `/Applications/RBS.app`). `None` outside any bundle.
pub fn enclosing_app(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for part in path.components() {
        out.push(part);
        if part
            .as_os_str()
            .to_str()
            .is_some_and(|s| s.len() > 4 && s.ends_with(".app"))
        {
            return Some(out);
        }
    }
    None
}

/// The app's display name from its bundle path (`/Applications/iTerm.app` gives
/// `iTerm`).
pub fn app_name(bundle: &Path) -> Option<String> {
    let name = bundle.file_name()?.to_str()?;
    Some(name.strip_suffix(".app").unwrap_or(name).to_owned())
}

/// The bundle of the first ancestor that is inside a bundle other than `own`, from
/// the nearest parent up: the app macOS most likely treats as responsible.
pub fn foreign_app(own: Option<&Path>, ancestors: &[PathBuf]) -> Option<PathBuf> {
    ancestors
        .iter()
        .filter_map(|a| enclosing_app(a))
        .find(|app| Some(app.as_path()) != own)
}

/// Whether `plist` (the text of an XML `Info.plist`) declares `key`.
pub fn plist_has_key(plist: &str, key: &str) -> bool {
    plist.contains(&format!("<key>{key}</key>"))
}

/// Whether this process is its own responsible app: its executable is inside a
/// bundle and no ancestor is inside another bundle. A prompt asked here is about RBS,
/// not about the terminal or app that started it.
pub fn is_own_app(exe: &Path, ancestors: &[PathBuf]) -> bool {
    enclosing_app(exe).is_some_and(|own| foreign_app(Some(&own), ancestors).is_none())
}

/// Whether asking for a permission may show the OS prompt without ending the
/// process: this executable is inside a bundle, no ancestor is inside another bundle,
/// and the bundle's `Info.plist` declares `key`.
pub fn prompt_is_safe(exe: &Path, ancestors: &[PathBuf], plist: Option<&str>, key: &str) -> bool {
    is_own_app(exe, ancestors) && plist.is_some_and(|p| plist_has_key(p, key))
}

/// This process as the OS sees it, read once by the caller.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AppIdentity {
    /// This executable.
    pub exe: PathBuf,
    /// The bundle this executable is inside, if any.
    pub bundle: Option<PathBuf>,
    /// The text of the bundle's `Contents/Info.plist`, if it could be read.
    pub plist: Option<String>,
    /// The executables of the parent processes, nearest first.
    pub ancestors: Vec<PathBuf>,
}

impl AppIdentity {
    /// Whether a prompt for the permission whose usage key is `key` is safe here.
    pub fn prompt_is_safe(&self, key: &str) -> bool {
        prompt_is_safe(&self.exe, &self.ancestors, self.plist.as_deref(), key)
    }

    /// Whether this process is its own responsible app (see [`is_own_app`]): a
    /// permission without a usage key (Input Monitoring) may prompt only then.
    pub fn is_own_app(&self) -> bool {
        is_own_app(&self.exe, &self.ancestors)
    }

    /// The app that most likely holds the grants when it is not this one.
    pub fn foreign_app(&self) -> Option<PathBuf> {
        foreign_app(self.bundle.as_deref(), &self.ancestors)
    }

    /// The bundle identifier from the plist (`CFBundleIdentifier`), if any.
    pub fn bundle_id(&self) -> Option<String> {
        let plist = self.plist.as_deref()?;
        let rest = plist.split("<key>CFBundleIdentifier</key>").nth(1)?;
        let start = rest.find("<string>")? + "<string>".len();
        let end = rest[start..].find("</string>")?;
        Some(rest[start..start + end].trim().to_owned())
    }
}

/// [`current`], read once for the life of the process (the executable, its bundle
/// and its parents do not change while it runs).
pub fn cached() -> &'static AppIdentity {
    static ID: std::sync::OnceLock<AppIdentity> = std::sync::OnceLock::new();
    ID.get_or_init(current)
}

/// Reads this process's identity: executable, bundle, plist and parent chain.
pub fn current() -> AppIdentity {
    let exe = std::env::current_exe().unwrap_or_default();
    let bundle = enclosing_app(&exe);
    let plist = bundle
        .as_ref()
        .and_then(|b| std::fs::read_to_string(b.join("Contents").join("Info.plist")).ok());
    AppIdentity {
        exe,
        bundle,
        plist,
        ancestors: ancestors(),
    }
}

/// The executables of the parent processes, nearest first, at most
/// [`MAX_ANCESTORS`]. Empty where the OS gives no reading.
pub fn ancestors() -> Vec<PathBuf> {
    #[cfg(unix)]
    {
        crate::unix::ancestors(MAX_ANCESTORS)
    }
    #[cfg(not(unix))]
    {
        Vec::new()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    const PLIST: &str = "<dict><key>CFBundleIdentifier</key><string>app.rbs.RBS</string>\
        <key>NSCameraUsageDescription</key><string>x</string></dict>";

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn a_bundle_started_by_launchd_may_prompt() {
        let exe = p("/Applications/RBS.app/Contents/MacOS/rbs");
        assert!(prompt_is_safe(
            &exe,
            &[p("/sbin/launchd")],
            Some(PLIST),
            "NSCameraUsageDescription"
        ));
    }

    #[test]
    fn a_missing_usage_key_never_prompts() {
        let exe = p("/Applications/RBS.app/Contents/MacOS/rbs");
        assert!(!prompt_is_safe(
            &exe,
            &[],
            Some(PLIST),
            "NSMicrophoneUsageDescription"
        ));
        assert!(!prompt_is_safe(&exe, &[], None, "NSCameraUsageDescription"));
    }

    #[test]
    fn a_bundle_started_from_a_terminal_does_not_prompt() {
        let exe = p("/Users/a/rbs/target/release/RBS.app/Contents/MacOS/rbs");
        let chain = [
            p("/bin/zsh"),
            p("/System/Applications/Utilities/Terminal.app/Contents/MacOS/Terminal"),
            p("/sbin/launchd"),
        ];
        assert!(!prompt_is_safe(
            &exe,
            &chain,
            Some(PLIST),
            "NSCameraUsageDescription"
        ));
    }

    #[test]
    fn a_plain_executable_never_prompts_and_names_its_starter() {
        let exe = p("/Users/a/rbs/target/release/rbs");
        let chain = [
            p("/Users/a/.cargo/bin/cargo"),
            p("/bin/zsh"),
            p("/Applications/iTerm.app/Contents/MacOS/iTerm2"),
        ];
        assert!(!prompt_is_safe(
            &exe,
            &chain,
            Some(PLIST),
            "NSCameraUsageDescription"
        ));
        let app = foreign_app(enclosing_app(&exe).as_deref(), &chain).unwrap();
        assert_eq!(app_name(&app).as_deref(), Some("iTerm"));
        assert!(!is_own_app(&exe, &chain));
    }

    #[test]
    fn a_bundle_started_by_launchd_is_its_own_app() {
        let exe = p("/Applications/RBS.app/Contents/MacOS/rbs");
        assert!(is_own_app(&exe, &[p("/sbin/launchd")]));
        let from_terminal = [p(
            "/System/Applications/Utilities/Terminal.app/Contents/MacOS/Terminal",
        )];
        assert!(!is_own_app(&exe, &from_terminal));
    }

    #[test]
    fn helpers_inside_the_own_bundle_are_not_foreign() {
        let own = p("/Applications/RBS.app");
        let chain = [p("/Applications/RBS.app/Contents/MacOS/rbs")];
        assert_eq!(foreign_app(Some(&own), &chain), None);
    }

    #[test]
    fn the_bundle_id_is_read_from_the_plist() {
        let id = AppIdentity {
            plist: Some(PLIST.to_owned()),
            ..AppIdentity::default()
        };
        assert_eq!(id.bundle_id().as_deref(), Some("app.rbs.RBS"));
        assert_eq!(AppIdentity::default().bundle_id(), None);
    }

    #[test]
    fn current_reads_without_failing() {
        let id = current();
        assert!(id.ancestors.len() <= MAX_ANCESTORS);
    }
}
