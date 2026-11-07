//! The Windows factory and its sessions (ADR-0070 decisions 4 to 7).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use rbs_core::capability::{CapabilitySet, CapabilityValue};
use rbs_core::media::{MemoryKind, PixelFormat};
use rbs_vcam::{
    CameraFormat, Handoff, HandoffReceiver, InstallState, SlotToken, VcamError, VcamStats,
    VirtualCameraConfig, VirtualCameraFactory, VirtualCameraSession, caps,
};
use rbs_vcam_mf_source::announced::{Announced, RATES};
use rbs_vcam_mf_source::pump;
use rbs_vcam_ring::header_info;
use windows::Win32::Foundation::ERROR_CANCELLED;
use windows::Win32::System::SystemInformation::GetTickCount64;

use super::ring::{WinRing, open};
use super::sys::{
    SETUP_EXE, SOURCE_DLL, class_key, current_user, format_path, hklm_string, install_dir,
    same_file, trusted_location,
};
use super::{camera, elevate};
use crate::sender::{RETRY_MS, RingSender};
use crate::{ID, LinkStore};

/// The Media Foundation virtual camera factory for the user running the app.
pub struct MfVcamFactory {
    caps: CapabilitySet,
    links: Box<dyn LinkStore>,
}

impl std::fmt::Debug for MfVcamFactory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MfVcamFactory").finish_non_exhaustive()
    }
}

fn os(code: windows_core::HRESULT, what: &str) -> VcamError {
    VcamError::Os {
        code: i64::from(code.0),
        message: what.to_owned(),
    }
}

fn win(e: &windows_core::Error, what: &str) -> VcamError {
    os(e.code(), &format!("{what}: {}", e.message()))
}

/// RBS runs from a folder whose DLL is never registered (not Program Files).
fn not_installed_here() -> VcamError {
    VcamError::Unsupported("Install RBS to add the virtual camera".into())
}

/// The announced format for `format`, or `InvalidConfig`.
fn announced(format: &CameraFormat) -> Result<Announced, VcamError> {
    if format.pixel != PixelFormat::Nv12 {
        return Err(VcamError::InvalidConfig("NV12 only".into()));
    }
    Announced::new(
        format.width,
        format.height,
        format.rate.per_second().num(),
        format.rate.per_second().den(),
    )
    .ok_or_else(|| VcamError::InvalidConfig(format!("{format:?}")))
}

impl MfVcamFactory {
    /// A factory that keeps the camera's symbolic link in `links`.
    pub fn new(links: Box<dyn LinkStore>) -> Self {
        let set = |v: &[&str]| CapabilityValue::Set(v.iter().map(|s| (*s).to_owned()).collect());
        let rates: Vec<String> = RATES.iter().map(u32::to_string).collect();
        let caps = CapabilitySet::new()
            .with(caps::INPUT_MEMORY, set(&["system"]))
            .with(caps::INPUT_FORMAT, set(&["nv12"]))
            .with(caps::MAX_WIDTH, CapabilityValue::Int(1920))
            .with(caps::MAX_HEIGHT, CapabilityValue::Int(1080))
            .with(
                caps::RATES,
                CapabilityValue::Set(rates.into_iter().collect()),
            )
            .with(caps::PERSISTENT, CapabilityValue::Flag(true));
        Self { caps, links }
    }

    fn user() -> Result<String, VcamError> {
        current_user().ok_or_else(|| VcamError::Os {
            code: 0,
            message: "could not read the user's SID".into(),
        })
    }

    /// Whether the user's class key and format file exist, and whether the class names
    /// this install's DLL.
    fn registration(sid: &str) -> Registration {
        let key = class_key(sid);
        if hklm_string(&key, Some("RbsVcamUser")).as_deref() != Some(sid)
            || !format_path(sid).is_some_and(|p| p.is_file())
        {
            return Registration::Missing;
        }
        let ours = install_dir().map(|d| d.join(SOURCE_DLL));
        let named = hklm_string(&format!("{key}\\InprocServer32"), None).map(PathBuf::from);
        match (ours, named) {
            (Some(a), Some(b)) if same_file(&a, &b) => Registration::Ours,
            _ => Registration::Other,
        }
    }

    fn state(&self, sid: &str) -> InstallState {
        if !camera::supported() {
            return InstallState::Unsupported {
                reason: "Needs Windows 11".into(),
            };
        }
        match Self::registration(sid) {
            Registration::Missing => InstallState::NotInstalled,
            Registration::Other => InstallState::NeedsUpdate {
                reason: "The virtual camera belongs to another RBS install".into(),
            },
            Registration::Ours => match self.links.load() {
                Some(link) if camera::device_exists(&link) => InstallState::Installed,
                _ => InstallState::NotInstalled,
            },
        }
    }

    fn run_setup(verb: &str, sid: &str) -> Result<(), VcamError> {
        let exe = install_dir()
            .map(|d| d.join(SETUP_EXE))
            .ok_or_else(|| VcamError::Unsupported("RBS install folder not found".into()))?;
        match elevate::run(&exe, &format!("{verb} {sid}")) {
            Ok(0) => Ok(()),
            Ok(code) if i64::from(code) == i64::from(super::setup::NOT_INSTALLED) => {
                Err(not_installed_here())
            }
            Ok(code) => Err(VcamError::Os {
                code: i64::from(code),
                message: format!("{SETUP_EXE} {verb} failed"),
            }),
            Err(e) if e.code() == ERROR_CANCELLED.to_hresult() => Err(VcamError::PermissionDenied(
                "Administrator approval was not given".into(),
            )),
            Err(e) => Err(win(&e, "starting the setup tool")),
        }
    }
}

enum Registration {
    Missing,
    Ours,
    Other,
}

impl VirtualCameraFactory for MfVcamFactory {
    fn id(&self) -> &'static str {
        ID
    }

    fn capabilities(&self) -> &CapabilitySet {
        &self.caps
    }

    fn status(&self) -> Result<InstallState, VcamError> {
        Ok(self.state(&Self::user()?))
    }

    fn install(&self) -> Result<InstallState, VcamError> {
        let sid = Self::user()?;
        match self.state(&sid) {
            InstallState::Installed => return Ok(InstallState::Installed),
            InstallState::Unsupported { reason } => return Err(VcamError::Unsupported(reason)),
            _ => {}
        }
        // The one prompt, only when the registration is missing or another install's,
        // and never for a DLL the setup tool would refuse.
        if !matches!(Self::registration(&sid), Registration::Ours) {
            if !install_dir().is_some_and(|d| trusted_location(&d.join(SOURCE_DLL))) {
                return Err(not_installed_here());
            }
            Self::run_setup("register", &sid)?;
        }
        match camera::add(&sid) {
            Ok(link) => self.links.save(link.as_deref()),
            Err(e) if e.code() == windows::Win32::Foundation::E_ACCESSDENIED => {
                return Err(VcamError::PermissionDenied(
                    "Allow desktop apps to use the camera in Windows Settings".into(),
                ));
            }
            Err(e) => return Err(win(&e, "MFCreateVirtualCamera")),
        }
        Ok(self.state(&sid))
    }

    fn uninstall(&self) -> Result<InstallState, VcamError> {
        let sid = Self::user()?;
        if camera::supported() && self.links.load().is_some() {
            camera::remove(&sid).map_err(|e| win(&e, "removing the camera"))?;
        }
        self.links.save(None);
        Ok(self.state(&sid))
    }

    fn set_format(&self, format: &CameraFormat) -> Result<bool, VcamError> {
        let wanted = announced(format)?;
        let sid = Self::user()?;
        let path = format_path(&sid).ok_or(VcamError::NotInstalled)?;
        write_format(&path, &wanted)?;
        // SAFETY: no preconditions.
        let now = unsafe { GetTickCount64() };
        let reading = open(&sid)
            .and_then(|(region, keep)| {
                let info = header_info(&region);
                drop(region);
                drop(keep);
                info
            })
            .is_some_and(|info| info.reader_active(now));
        Ok(!reading)
    }

    fn open(
        &self,
        config: &VirtualCameraConfig,
    ) -> Result<Box<dyn VirtualCameraSession>, VcamError> {
        let sid = Self::user()?;
        if self.state(&sid) != InstallState::Installed {
            return Err(VcamError::NotInstalled);
        }
        let format = announced(&config.format)?;
        if config.memory != MemoryKind::System {
            return Err(VcamError::InvalidConfig("system memory only".into()));
        }
        Ok(Box::new(MfSession::start(*config, &format, &sid)?))
    }
}

/// Writes the format file in one call at offset 0, with write-data access only (what
/// its access list grants the user).
fn write_format(path: &std::path::Path, format: &Announced) -> Result<(), VcamError> {
    use std::os::windows::fs::{FileExt, OpenOptionsExt};
    /// `FILE_WRITE_DATA`.
    const WRITE_DATA: u32 = 0x0002;
    let file = std::fs::OpenOptions::new()
        .access_mode(WRITE_DATA)
        .open(path)
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => VcamError::NotInstalled,
            _ => VcamError::Os {
                code: e.raw_os_error().map_or(0, i64::from),
                message: format!("writing the camera format: {e}"),
            },
        })?;
    let bytes = format.to_value();
    match file.seek_write(&bytes, 0) {
        Ok(n) if n == bytes.len() => Ok(()),
        Ok(_) => Err(VcamError::Os {
            code: 0,
            message: "short write of the camera format".into(),
        }),
        Err(e) => Err(VcamError::Os {
            code: e.raw_os_error().map_or(0, i64::from),
            message: format!("writing the camera format: {e}"),
        }),
    }
}

/// A sending session: the hand-off plus a sender thread writing the ring.
pub struct MfSession {
    handoff: Handoff,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl MfSession {
    fn start(
        config: VirtualCameraConfig,
        format: &Announced,
        sid: &str,
    ) -> Result<Self, VcamError> {
        let layout =
            pump::layout(format).map_err(|_| VcamError::InvalidConfig(format!("{format:?}")))?;
        let (mut handoff, rx) = Handoff::new(config);
        let stop = Arc::new(AtomicBool::new(false));
        let sender = RingSender::new(WinRing::new(sid), layout, Arc::clone(handoff.counters()));
        let thread = {
            let stop = Arc::clone(&stop);
            std::thread::Builder::new()
                .name("rbs-vcam-mf".into())
                .spawn(move || run(sender, rx, stop))
                .map_err(|e| VcamError::Os {
                    code: 0,
                    message: format!("spawn: {e}"),
                })?
        };
        handoff.set_sender(thread.thread().clone());
        Ok(Self {
            handoff,
            stop,
            thread: Some(thread),
        })
    }
}

fn run(mut sender: RingSender<WinRing>, rx: HandoffReceiver, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Acquire) {
        // SAFETY: no preconditions.
        let now = unsafe { GetTickCount64() };
        match rx.take_newest() {
            Some(frame) => {
                sender.send(&frame, now);
            }
            None => {
                sender.idle(now);
                rx.wait(Duration::from_millis(RETRY_MS));
            }
        }
    }
}

impl VirtualCameraSession for MfSession {
    fn reserve(&self) -> Option<SlotToken> {
        self.handoff.reserve()
    }

    fn send(&self, slot: SlotToken, frame: rbs_core::media::VideoFrame) -> Result<(), VcamError> {
        self.handoff.send(slot, frame)
    }

    fn stats(&self) -> VcamStats {
        self.handoff.stats()
    }

    fn counters(&self) -> Option<std::sync::Arc<rbs_vcam::VcamCounters>> {
        Some(std::sync::Arc::clone(self.handoff.counters()))
    }
}

impl Drop for MfSession {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            t.thread().unpark();
            let _ = t.join();
        }
    }
}
