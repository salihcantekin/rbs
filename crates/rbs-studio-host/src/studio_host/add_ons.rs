//! Add-ons on the control thread (ADR-0145, add-ons spec "Control API"): the
//! `addon.*` methods, the install thread and the `addon.changed` topic.
//!
//! Nothing runs until a call asks (P13): the list reads each entry's directory when
//! it is asked for, and an install has its own thread only while it runs.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rbs_add_ons::{AddOns, Entry, FailReason, InstallControl, InstallError, Installed, Status};
use rbs_api::event::{EventPayload, StateKey, Topic};
use rbs_api::types as api;
use rbs_api::{ApiError, ErrorKind};

use super::EngineRuntime;
use super::{Host, Msg, Update};
use crate::add_ons::{AddOnsSetup, BROWSER_ENGINE};

/// What an install thread reports.
#[derive(Debug)]
pub(super) enum AddOnMsg {
    /// Downloading, verifying or installing.
    Progress(Status),
    /// The install ended.
    Done(Result<Installed, InstallError>),
}

/// One running install.
struct Run {
    cancel: Arc<AtomicBool>,
    shown: Status,
    /// Update: clean older versions once the install succeeded (add-ons spec,
    /// "Clean").
    clean_after: bool,
}

/// The add-ons the studio knows and what runs.
pub(super) struct AddOnsHost {
    catalog: Vec<Entry>,
    /// `None`: this computer has no place for add-ons (no home folder), or the app
    /// gave none (tests that do not use them); installs are refused.
    setup: Option<AddOnsSetup>,
    runs: Vec<Option<Run>>,
    failed: Vec<Option<FailReason>>,
}

impl AddOnsHost {
    /// With `setup`, or the built-in catalog and no storage.
    pub(super) fn new(setup: Option<AddOnsSetup>) -> Self {
        let catalog = setup.as_ref().map_or_else(
            || crate::add_ons::catalog(crate::add_ons::target()),
            |s| s.catalog.clone(),
        );
        let n = catalog.len();
        Self {
            catalog,
            setup,
            runs: (0..n).map(|_| None).collect(),
            failed: vec![None; n],
        }
    }

    fn store(&self) -> Option<AddOns> {
        self.setup
            .as_ref()
            .map(|s| AddOns::new(s.root.clone(), Arc::clone(&s.fs)))
    }

    /// The catalog's version of the add-on `id` when it is installed with its marker
    /// (reads the disk; add-ons spec "Resolving").
    pub(super) fn installed(&self, id: &str) -> Option<Installed> {
        let entry = self.catalog.iter().find(|e| e.id.as_str() == id)?;
        self.store()?.installed(entry)
    }

    /// The installed browser engine as the runner needs it (reads the disk).
    pub(super) fn browser_engine(&self) -> Option<EngineRuntime> {
        self.installed(BROWSER_ENGINE).map(|i| EngineRuntime {
            root: i.path,
            library_dir: i.library_dir,
        })
    }

    fn status(&self, index: usize) -> Status {
        if let Some(run) = &self.runs[index] {
            return run.shown.clone();
        }
        if let Some(reason) = self.failed[index] {
            return Status::Failed { reason };
        }
        self.store()
            .map_or(Status::NotInstalled, |s| s.status(&self.catalog[index]))
    }

    fn index(&self, id: &str) -> Result<usize, ApiError> {
        self.catalog
            .iter()
            .position(|e| e.id.as_str() == id)
            .ok_or_else(|| ApiError::new(ErrorKind::NotFound, format!("no add-on `{id}`")))
    }

    fn info(&self, index: usize) -> api::AddOnInfo {
        let e = &self.catalog[index];
        api::AddOnInfo {
            id: e.id.as_str().to_owned(),
            kind: match e.kind {
                rbs_add_ons::Kind::Runtime => "runtime".into(),
            },
            name: e.name.clone(),
            summary: e.summary.clone(),
            version: e.version.as_str().to_owned(),
            size: e.artifact.as_ref().map(|a| a.size),
            unpacked_size: e.artifact.as_ref().map(|a| a.unpacked_size),
            available: e.artifact.is_some(),
            status: api_status(&self.status(index)),
        }
    }
}

/// The wire form of a status.
pub(super) fn api_status(s: &Status) -> api::AddOnStatus {
    match s {
        Status::NotInstalled => api::AddOnStatus::NotInstalled,
        Status::Downloading { bytes, total } => api::AddOnStatus::Downloading {
            bytes: *bytes,
            total: *total,
        },
        Status::Verifying => api::AddOnStatus::Verifying,
        Status::Installing => api::AddOnStatus::Installing,
        Status::Installed { version } => api::AddOnStatus::Installed {
            version: version.clone(),
        },
        Status::UpdateAvailable {
            installed,
            available,
        } => api::AddOnStatus::UpdateAvailable {
            installed: installed.clone(),
            available: available.clone(),
        },
        Status::Failed { reason } => api::AddOnStatus::Failed {
            reason: match reason {
                FailReason::Network => api::AddOnFailReason::Network,
                FailReason::HostNotAllowed => api::AddOnFailReason::HostNotAllowed,
                FailReason::TooLarge => api::AddOnFailReason::TooLarge,
                FailReason::HashMismatch => api::AddOnFailReason::HashMismatch,
                FailReason::UnsafeArchive => api::AddOnFailReason::UnsafeArchive,
                FailReason::NoSpace => api::AddOnFailReason::NoSpace,
                FailReason::Io => api::AddOnFailReason::Io,
            },
        },
    }
}

fn busy(what: &str) -> ApiError {
    ApiError::new(ErrorKind::Busy, what.to_owned())
}

fn refused(e: &InstallError) -> ApiError {
    match e {
        InstallError::Busy => busy("another install or removal of this add-on is running"),
        other => ApiError::new(ErrorKind::Failed, other.to_string()),
    }
}

impl Host {
    /// `addon.list`.
    pub(super) fn addon_list(&self) -> api::AddOnList {
        api::AddOnList {
            add_ons: (0..self.add_ons.catalog.len())
                .map(|i| self.add_ons.info(i))
                .collect(),
        }
    }

    fn publish_addon(&mut self, index: usize, notify: &dyn Fn(Update)) {
        let changed = api::AddOnChanged {
            id: self.add_ons.catalog[index].id.as_str().to_owned(),
            status: api_status(&self.add_ons.status(index)),
        };
        let key = StateKey {
            topic: Topic::AddonChanged,
            object: u32::try_from(index).ok(),
        };
        notify(Update::AddOnChanged(changed.clone()));
        self.events
            .changed(key, || EventPayload::AddonChanged(changed));
    }

    /// `addon.install`: starts the install thread and answers at once.
    pub(super) fn addon_install(
        &mut self,
        id: &str,
        notify: &dyn Fn(Update),
    ) -> Result<(), ApiError> {
        let index = self.add_ons.index(id)?;
        let entry = self.add_ons.catalog[index].clone();
        let Some(total) = entry.artifact.as_ref().map(|a| a.size) else {
            let mut data = serde_json::Map::new();
            data.insert("reason".into(), "not_available".into());
            return Err(ApiError::with_data(
                ErrorKind::InvalidParams,
                format!("`{id}` has no download for this computer"),
                data,
            ));
        };
        if self.add_ons.runs[index].is_some() {
            return Err(busy("this add-on is already being installed"));
        }
        let (Some(setup), Some(store)) = (self.add_ons.setup.clone(), self.add_ons.store()) else {
            return Err(ApiError::new(
                ErrorKind::Failed,
                "this computer has no folder for add-ons",
            ));
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let inbox = self.inbox.clone();
        let post = move |msg| {
            if let Some(tx) = inbox.upgrade() {
                let _ = tx.send(Msg::AddOn { index, msg });
            }
        };
        let thread_cancel = Arc::clone(&cancel);
        std::thread::Builder::new()
            .name("rbs-add-on-install".into())
            .spawn(move || {
                let mut progress = |s: Status| post(AddOnMsg::Progress(s));
                let result = store.install(
                    &entry,
                    setup.fetch.as_ref(),
                    setup.unpack.as_ref(),
                    InstallControl {
                        cancel: &thread_cancel,
                        clock: setup.clock.as_ref(),
                        progress: &mut progress,
                    },
                );
                post(AddOnMsg::Done(result));
            })
            .map_err(|e| {
                ApiError::new(ErrorKind::Failed, format!("cannot start the install: {e}"))
            })?;
        self.add_ons.failed[index] = None;
        self.add_ons.runs[index] = Some(Run {
            cancel,
            shown: Status::Downloading { bytes: 0, total },
            clean_after: false,
        });
        self.publish_addon(index, notify);
        Ok(())
    }

    /// `addon.cancel`: no-op when nothing runs. The status returns to what it was when
    /// the install thread ends.
    pub(super) fn addon_cancel(&mut self, id: &str) -> Result<(), ApiError> {
        let index = self.add_ons.index(id)?;
        if let Some(run) = &self.add_ons.runs[index] {
            run.cancel.store(true, Ordering::Release);
        }
        Ok(())
    }

    /// `addon.clean`: removes versions older than the one before the catalog's.
    pub(super) fn addon_clean(
        &mut self,
        id: &str,
        notify: &dyn Fn(Update),
    ) -> Result<(), ApiError> {
        let index = self.add_ons.index(id)?;
        if self.add_ons.runs[index].is_some() {
            return Err(busy("this add-on is being installed"));
        }
        // Clean keeps the catalog's version, the only one a consumer here loads.
        if let Some(store) = self.add_ons.store() {
            store
                .clean(&self.add_ons.catalog[index])
                .map_err(|e| refused(&e))?;
        }
        self.publish_addon(index, notify);
        Ok(())
    }

    /// `addon.remove`: every version; `busy` while this process uses the add-on.
    pub(super) fn addon_remove(
        &mut self,
        id: &str,
        notify: &dyn Fn(Update),
    ) -> Result<(), ApiError> {
        let index = self.add_ons.index(id)?;
        if self.add_ons.runs[index].is_some() {
            return Err(busy("this add-on is being installed"));
        }
        if id == BROWSER_ENGINE && self.browser_engine_in_use() {
            return Err(busy(
                "a browser source uses the browser engine; hide or remove it first",
            ));
        }
        if let Some(store) = self.add_ons.store() {
            store
                .remove(&self.add_ons.catalog[index])
                .map_err(|e| refused(&e))?;
        }
        self.add_ons.failed[index] = None;
        self.add_on_changed(index, notify);
        self.publish_addon(index, notify);
        Ok(())
    }

    /// A report of the install thread of entry `index`.
    pub(super) fn addon_msg(&mut self, index: usize, msg: AddOnMsg, notify: &dyn Fn(Update)) {
        let Some(run) = self.add_ons.runs.get_mut(index).and_then(Option::as_mut) else {
            return;
        };
        match msg {
            AddOnMsg::Progress(status) => run.shown = status,
            AddOnMsg::Done(result) => {
                let clean_after = run.clean_after;
                self.add_ons.runs[index] = None;
                match result {
                    Ok(_) => {
                        self.add_ons.failed[index] = None;
                        if clean_after
                            && let Some(store) = self.add_ons.store()
                            && let Err(e) = store.clean(&self.add_ons.catalog[index])
                        {
                            rbs_core::warn!(
                                rbs_core::log::Area::AddOns,
                                "cleaning old versions of {} failed: {e}",
                                self.add_ons.catalog[index].id
                            );
                        }
                        self.add_on_changed(index, notify);
                    }
                    Err(e) => {
                        if let Some(reason) = e.fail_reason() {
                            rbs_core::error!(
                                rbs_core::log::Area::AddOns,
                                "installing {} failed: {e}",
                                self.add_ons.catalog[index].id
                            );
                            self.add_ons.failed[index] = Some(reason);
                        }
                    }
                }
            }
        }
        self.publish_addon(index, notify);
    }

    /// `addon.update`: installs the newest version, then cleans the older ones from the
    /// install's done event (add-ons spec, ADR-0164 decision 6). A failed or cancelled
    /// install keeps the old version.
    pub(super) fn addon_update(
        &mut self,
        id: &str,
        notify: &dyn Fn(Update),
    ) -> Result<(), ApiError> {
        self.addon_install(id, notify)?;
        if let Ok(index) = self.add_ons.index(id)
            && let Some(run) = self.add_ons.runs[index].as_mut()
        {
            run.clean_after = true;
        }
        Ok(())
    }

    /// An add-on was installed or removed: its consumers look again (add-ons spec,
    /// "Browser engine consumer").
    fn add_on_changed(&mut self, index: usize, notify: &dyn Fn(Update)) {
        if self.add_ons.catalog[index].id.as_str() == BROWSER_ENGINE {
            self.browser_engine_changed(notify);
        }
    }
}

impl Host {
    /// True while a browser runner started by this process is alive.
    fn browser_engine_in_use(&self) -> bool {
        self.browser.in_use()
    }

    /// Reads the engine again (Add Source › Browser, ADR-0145 decision 6) and says
    /// whether a browser source could not start for want of it.
    pub(super) fn browser_needs_engine(&mut self) -> bool {
        self.engine_slot.set(self.add_ons.browser_engine());
        self.browser.needs_engine()
    }

    /// The browser engine was installed or removed: the runner's slot follows, and
    /// every browser source waiting for the engine starts again with no restart
    /// (add-ons spec "Browser engine consumer").
    fn browser_engine_changed(&mut self, notify: &dyn Fn(Update)) {
        self.engine_slot.set(self.add_ons.browser_engine());
        if self.studio.browser_engine_changed() > 0 {
            self.apply(notify);
        }
    }
}
