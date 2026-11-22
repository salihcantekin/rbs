//! The applications behind `Application` endpoints (crate docs, "Endpoints"): the
//! outermost `.app` bundle of a process, its helpers, and the web apps a browser
//! runs. Cold paths only (`enumerate`, `start`); nothing here runs per cycle.

use std::collections::HashMap;

use objc2_core_audio::{
    AudioObjectID, kAudioHardwarePropertyProcessObjectList, kAudioObjectPropertyScopeGlobal,
    kAudioProcessPropertyPID,
};
use objc2_foundation::{NSBundle, NSString};

use super::{SYSTEM, get, get_ids, process_object};

/// An application bundle: its path and its folder name without `.app`.
pub(super) struct App {
    pub(super) bundle: String,
    pub(super) name: String,
}

fn process_path(pid: i32) -> Option<String> {
    let mut buffer = vec![0_u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: `buffer` is writable for its whole length, which is passed as the size.
    let len = unsafe { libc::proc_pidpath(pid, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
    let len = usize::try_from(len).ok().filter(|l| *l > 0)?;
    buffer.truncate(len);
    String::from_utf8(buffer).ok()
}

/// The application `pid` belongs to, or `None` for a process outside any bundle.
pub(super) fn application_of(pid: i32) -> Option<App> {
    let path = process_path(pid)?;
    let (bundle, name) = crate::app_bundle(&path)?;
    Some(App {
        bundle: bundle.to_owned(),
        name: name.to_owned(),
    })
}

/// The bundle identifier in a bundle's `Info.plist`.
pub(super) fn bundle_id(bundle: &str) -> Option<String> {
    let bundle = NSBundle::bundleWithPath(&NSString::from_str(bundle))?;
    bundle.bundleIdentifier().map(|id| id.to_string())
}

/// Every process the HAL knows, with its pid, RBS's own left out.
fn hal_processes() -> Vec<(AudioObjectID, i32)> {
    let own = std::process::id() as i32;
    get_ids(
        SYSTEM,
        kAudioHardwarePropertyProcessObjectList,
        kAudioObjectPropertyScopeGlobal,
    )
    .into_iter()
    .filter_map(|object| {
        get::<i32>(
            object,
            kAudioProcessPropertyPID,
            kAudioObjectPropertyScopeGlobal,
        )
        .filter(|pid| *pid > 0 && *pid != own)
        .map(|pid| (object, pid))
    })
    .collect()
}

/// The process objects a tap of `pid`'s application reads: `pid`'s own first when the
/// HAL has one, then every process of the same bundle, then, for a browser's web app,
/// the processes of the browser that plays its sound (`crate::belongs_to`). Empty
/// when none of them has an audio object.
pub(super) fn application_processes(pid: i32) -> Vec<AudioObjectID> {
    let mut objects: Vec<AudioObjectID> = process_object(pid).into_iter().collect();
    let Some(app) = application_of(pid) else {
        return objects;
    };
    let family = bundle_id(&app.bundle);
    let mut ids: HashMap<String, Option<String>> = HashMap::new();
    for (object, other) in hal_processes() {
        if objects.contains(&object) {
            continue;
        }
        let Some(theirs) = application_of(other) else {
            continue;
        };
        let same = theirs.bundle == app.bundle
            || family.as_deref().is_some_and(|family| {
                ids.entry(theirs.bundle.clone())
                    .or_insert_with(|| bundle_id(&theirs.bundle))
                    .as_deref()
                    .is_some_and(|parent| crate::belongs_to(family, parent))
            });
        if same {
            objects.push(object);
        }
    }
    objects
}

/// Every pid on the system.
fn all_pids() -> Vec<i32> {
    // SAFETY: a null buffer asks only for the number of pids.
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    let Ok(count) = usize::try_from(count) else {
        return Vec::new();
    };
    // Room for processes started between the two calls.
    let mut pids = vec![0_i32; count + 64];
    let bytes = i32::try_from(pids.len() * size_of::<i32>()).unwrap_or(i32::MAX);
    // SAFETY: `pids` is writable for `bytes` bytes.
    let got = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
    pids.truncate(usize::try_from(got).unwrap_or(0).min(pids.len()));
    pids.retain(|pid| *pid > 0);
    pids
}

/// The running applications whose bundle id belongs to one of `families` (a playing
/// browser's web apps), one pid and name per bundle, leaving out `listed` bundles.
pub(super) fn family_members(families: &[String], listed: &[String]) -> Vec<(i32, String)> {
    if families.is_empty() {
        return Vec::new();
    }
    let mut seen: Vec<String> = listed.to_vec();
    let mut found = Vec::new();
    for pid in all_pids() {
        let Some(app) = application_of(pid) else {
            continue;
        };
        if seen.contains(&app.bundle) {
            continue;
        }
        seen.push(app.bundle.clone());
        let member = bundle_id(&app.bundle)
            .is_some_and(|id| families.iter().any(|f| crate::belongs_to(&id, f)));
        if member {
            found.push((pid, app.name));
        }
    }
    found
}
