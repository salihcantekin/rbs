//! `import.obs.scan`, `import.obs.check` and `import.obs.run` (control API 1.82,
//! ADR-0131 decision 12). Collections and profiles are named as `scan` found them,
//! never by path; files are read only inside OBS's settings folders. `run` writes on
//! the user data writer thread, then the stream key goes to this thread's key store
//! and the UI reloads its libraries and shortcuts.

use rbs_api::Response;
use rbs_api::error::{ApiError, ErrorKind};
use rbs_api::request::ObsImport;
use rbs_api::types as api;

use super::{Host, Update};
use crate::obs_import::{self, ApiSource, ApiSourceError};

fn read(p: &ObsImport) -> Result<ApiSource, ApiError> {
    obs_import::api_source(&p.collection, p.profile.as_deref()).map_err(|e| match e {
        ApiSourceError::NotFound(m) => ApiError::new(ErrorKind::NotFound, m),
        ApiSourceError::Unreadable(m) => ApiError::new(ErrorKind::Failed, m),
    })
}

fn plan(p: &ObsImport) -> Result<rbs_import_obs::ImportPlan, ApiError> {
    let source = read(p)?;
    obs_import::plan_api(&source, p.choose.as_ref())
        .map_err(|m| ApiError::new(ErrorKind::InvalidParams, m.to_string()))
}

/// `import.obs.scan`.
pub(super) fn scan() -> Result<Response, ApiError> {
    let (collections, profiles) = obs_import::api_scan();
    Ok(Response::ObsScan(api::ObsScan {
        collections,
        profiles,
    }))
}

/// `import.obs.check`: nothing is written.
pub(super) fn check(p: &ObsImport) -> Result<Response, ApiError> {
    Ok(Response::ObsCheck(obs_import::api_check(&plan(p)?)))
}

impl Host {
    /// `import.obs.run`: a new scene collection and output profile, the stream key and
    /// the hotkeys, as the wizard's Import does.
    pub(super) fn import_obs(
        &mut self,
        p: &ObsImport,
        notify: &dyn Fn(Update),
    ) -> Result<Response, ApiError> {
        let saver = self.autosave.as_ref().ok_or_else(|| {
            ApiError::new(
                ErrorKind::Failed,
                "no user data is loaded, so nothing can be imported",
            )
        })?;
        let plan = plan(p)?;
        let report = obs_import::api_check(&plan);
        let (written, key) = saver
            .import_obs(plan)
            .map_err(|m| ApiError::new(ErrorKind::Failed, format!("nothing was imported: {m}")))?;
        let mut notes: Vec<api::ObsReportEntry> =
            written.notes.iter().map(obs_import::api_entry).collect();
        if let Some((account, key)) = key
            && let Err(e) = self.keys.write(&account, &key)
        {
            notes.push(api::ObsReportEntry {
                outcome: "skipped".into(),
                area: "profile".into(),
                obs_name: "Stream key".into(),
                obs_type: String::new(),
                scenes: Vec::new(),
                reason: format!(
                    "The stream key could not be saved in {}: {e}.",
                    self.keys.place()
                ),
                hint: Some("Enter it in Settings › Stream.".into()),
            });
        }
        notify(Update::ObsImported {
            shortcuts: written.shortcuts_added > 0,
        });
        let entry = |(id, name): &(rbs_user_data::EntryId, String)| api::LibraryEntry {
            id: id.as_str().to_owned(),
            name: name.clone(),
        };
        Ok(Response::ObsRun(api::ObsRun {
            collection: entry(&written.collection),
            profile: written.profile.as_ref().map(entry),
            shortcuts_added: u32::try_from(written.shortcuts_added).unwrap_or(u32::MAX),
            report,
            notes,
        }))
    }
}
