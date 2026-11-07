//! Which user a requested class belongs to, and the format it announces (ADR-0070
//! decisions 2 and 3). Reads `HKLM` explicitly (64-bit view), never `HKCR`, reads the
//! user's format file, and only ever reads.

use std::io::Read;
use std::path::PathBuf;

use windows::Win32::Foundation::{ERROR_SUCCESS, WIN32_ERROR};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RRF_SUBKEY_WOW6464KEY, RegGetValueW,
};
use windows::Win32::UI::Shell::{FOLDERID_ProgramData, KF_FLAG_DEFAULT, SHGetKnownFolderPath};
use windows_core::{GUID, HSTRING, PCWSTR};

use crate::announced::{Announced, FORMAT_LEN};
use crate::clsid;

/// What the source needs to run for one user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourceConfig {
    /// The user's SID string.
    pub(crate) sid: String,
    /// The format to announce.
    pub(crate) format: Announced,
}

/// The class key of `clsid` under `root` (`HKLM` outside tests).
fn class_key(clsid: &GUID) -> String {
    format!("SOFTWARE\\Classes\\CLSID\\{}", braced(clsid))
}

/// `{XXXXXXXX-...}` of a GUID, upper case (the registry form).
pub(crate) fn braced(g: &GUID) -> String {
    clsid::braced(&to_bytes(g))
}

/// A GUID's 16 bytes in network order (RFC 9562), as [`clsid::for_sid`] returns them.
pub(crate) fn to_bytes(g: &GUID) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[0..4].copy_from_slice(&g.data1.to_be_bytes());
    b[4..6].copy_from_slice(&g.data2.to_be_bytes());
    b[6..8].copy_from_slice(&g.data3.to_be_bytes());
    b[8..16].copy_from_slice(&g.data4);
    b
}

/// The GUID for 16 bytes in network order.
pub(crate) fn from_bytes(b: &[u8; 16]) -> GUID {
    GUID {
        data1: u32::from_be_bytes([b[0], b[1], b[2], b[3]]),
        data2: u16::from_be_bytes([b[4], b[5]]),
        data3: u16::from_be_bytes([b[6], b[7]]),
        data4: [b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]],
    }
}

/// `LocalService`, the account Frame Server runs the source in.
pub(crate) const LOCAL_SERVICE: &str = "S-1-5-19";
/// `SYSTEM` (Frame Server Monitor).
pub(crate) const SYSTEM: &str = "S-1-5-18";

/// Whether `sid` has the form of a user account: local or domain (`S-1-5-21-` and four
/// numbers) or Entra ID (`S-1-12-1-` and four numbers). Group and well-known SIDs are
/// refused (ADR-0070 decision 2).
pub(crate) fn is_user_sid(sid: &str) -> bool {
    let Some(rest) = sid
        .strip_prefix("S-1-5-21-")
        .or_else(|| sid.strip_prefix("S-1-12-1-"))
    else {
        return false;
    };
    let parts: Vec<&str> = rest.split('-').collect();
    parts.len() == 4
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.len() <= 10 && p.bytes().all(|c| c.is_ascii_digit()))
}

/// The user `clsid` belongs to, if it is an RBS user class under `root`: its key
/// names a user SID and the CLSID is the one derived from it. `None` otherwise.
pub(crate) fn class_user(root: HKEY, clsid: &GUID) -> Option<String> {
    let sid = read_string(root, &class_key(clsid), "RbsVcamUser")?;
    (is_user_sid(&sid) && from_bytes(&clsid::for_sid(&sid)) == *clsid).then_some(sid)
}

/// Whether a process running as `caller` may use `class_user`'s class: the user
/// themselves, Frame Server (`LocalService`) or Frame Server Monitor (`SYSTEM`)
/// (ADR-0070 decision 2; it stops casual use only, see the threat there).
pub(crate) fn caller_allowed(caller: &str, class_user: &str) -> bool {
    caller == class_user || caller == LOCAL_SERVICE || caller == SYSTEM
}

/// The user of `clsid` if the calling process, running as `caller`, may use it.
pub(crate) fn lookup(root: HKEY, clsid: &GUID, caller: &str) -> Option<String> {
    let sid = class_user(root, clsid)?;
    caller_allowed(caller, &sid).then_some(sid)
}

/// What the source needs for `sid`, with the format read from the user's file now:
/// called at each activation (ADR-0070 decisions 3 and 4).
pub(crate) fn source_config(sid: &str) -> SourceConfig {
    let format = program_data()
        .map(|dir| read_format(dir.join(crate::announced::format_file(sid))))
        .unwrap_or_default();
    SourceConfig {
        sid: sid.to_owned(),
        format,
    }
}

/// `%ProgramData%`, from the shell's known folder (not the environment, which the
/// calling process controls).
fn program_data() -> Option<PathBuf> {
    // SAFETY: the returned string is CoTaskMemAlloc'd and freed below.
    let p = unsafe { SHGetKnownFolderPath(&FOLDERID_ProgramData, KF_FLAG_DEFAULT, None) }.ok()?;
    // SAFETY: a null-terminated string from the call above.
    let s = unsafe { p.to_string() }.ok();
    // SAFETY: allocated by `SHGetKnownFolderPath`, freed once.
    unsafe { CoTaskMemFree(Some(p.0.cast())) };
    s.map(PathBuf::from)
}

/// The format in the file at `path`, or the default (ADR-0070 decision 3). Opens it
/// read-only and reads at most [`FORMAT_LEN`] bytes.
fn read_format(path: PathBuf) -> Announced {
    let mut buf = [0u8; FORMAT_LEN];
    let mut len = 0;
    if let Ok(file) = std::fs::File::open(path) {
        let mut file = file.take(FORMAT_LEN as u64);
        while len < FORMAT_LEN {
            match file.read(&mut buf[len..]) {
                Ok(0) | Err(_) => break,
                Ok(n) => len += n,
            }
        }
    }
    Announced::from_value(&buf[..len])
}

/// A `REG_SZ` value, at most 255 characters.
fn read_string(root: HKEY, key: &str, name: &str) -> Option<String> {
    let mut buf = [0u16; 256];
    let mut len = size_of_val(&buf) as u32;
    let r = get_value(
        root,
        key,
        name,
        RRF_RT_REG_SZ.0,
        buf.as_mut_ptr().cast(),
        &mut len,
    );
    if r != ERROR_SUCCESS {
        return None;
    }
    let chars = (len as usize / 2).min(buf.len());
    let s = String::from_utf16(&buf[..chars]).ok()?;
    Some(s.trim_end_matches('\0').to_owned())
}

fn get_value(
    root: HKEY,
    key: &str,
    name: &str,
    kind: u32,
    data: *mut core::ffi::c_void,
    len: &mut u32,
) -> WIN32_ERROR {
    let key = HSTRING::from(key);
    let name = HSTRING::from(name);
    // SAFETY: null-terminated strings; `data` holds `*len` bytes (the callers' fixed
    // buffers), and `RegGetValueW` writes at most that many.
    unsafe {
        RegGetValueW(
            root,
            PCWSTR(key.as_ptr()),
            PCWSTR(name.as_ptr()),
            windows::Win32::System::Registry::REG_ROUTINE_FLAGS(kind | RRF_SUBKEY_WOW6464KEY.0),
            None,
            Some(data),
            Some(len),
        )
    }
}

/// The root the DLL reads: `HKLM`. Tests redirect it with `RegOverridePredefKey`.
pub(crate) fn root() -> HKEY {
    HKEY_LOCAL_MACHINE
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn only_user_account_sids_pass() {
        assert!(is_user_sid("S-1-5-21-1004336348-1177238915-682003330-1001"));
        for bad in [
            "S-1-1-0",
            "S-1-5-19",
            "S-1-5-32-544",
            "S-1-5-21-1-2-3",
            "S-1-5-21-1-2-3-4-5",
            "S-1-5-21-1-2-x-1001",
            "S-1-5-21-1-2--1001",
            "S-1-12-1-1-2-3",
            "S-1-12-2-1-2-3-4",
            "",
        ] {
            assert!(!is_user_sid(bad), "{bad}");
        }
        assert!(is_user_sid(
            "S-1-12-1-3570597712-1263000215-2283146672-3291235463"
        ));
    }

    #[test]
    fn only_the_user_and_frame_server_may_use_a_class() {
        let user = "S-1-5-21-1-2-3-1001";
        assert!(caller_allowed(user, user));
        assert!(caller_allowed(LOCAL_SERVICE, user));
        assert!(caller_allowed(SYSTEM, user));
        assert!(!caller_allowed("S-1-5-21-1-2-3-1002", user));
        assert!(!caller_allowed("S-1-5-20", user));
    }

    #[test]
    fn the_format_file_gives_its_format_or_the_default() {
        let dir = std::env::temp_dir().join(format!("rbs-vcam-fmt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("user.fmt");
        let wanted = Announced::new(640, 360, 15, 1).unwrap();
        std::fs::write(&path, wanted.to_value()).unwrap();
        assert_eq!(read_format(path.clone()), wanted);
        std::fs::write(&path, &wanted.to_value()[..8]).unwrap();
        assert_eq!(read_format(path.clone()), Announced::default());
        let mut longer = wanted.to_value().to_vec();
        longer.extend_from_slice(&[0; 8]);
        std::fs::write(&path, &longer).unwrap();
        assert_eq!(read_format(path.clone()), wanted, "reads at most 16 bytes");
        assert_eq!(read_format(dir.join("missing.fmt")), Announced::default());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn guid_bytes_round_trip_in_network_order() {
        let b = clsid::for_sid("S-1-5-21-1-2-3-1001");
        let g = from_bytes(&b);
        assert_eq!(to_bytes(&g), b);
        assert_eq!(braced(&g), "{AA0E7BC1-FADC-559C-92B3-0272623DF480}");
        assert_eq!(g.data1, 0xAA0E_7BC1);
    }
}
