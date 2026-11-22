//! What a kind can capture and adding one of it, sent as `source.targets` and
//! `source.add` (ADR-0168 step 4): Add source, the Source tab and Settings › Global
//! sources. A picker entry runs the OS picker on the host (`Command::PickSource`).

use rbs_api::Command as C;
use rbs_api::request as rq;
use rbs_api::{ApiError, Response};
use rbs_app::studio_host::{ApiReply, Command, SourceKind, Target, kind_for_api, kind_from_api};

use super::StudioUi;

/// The ticket of every `source.targets` call: the answer names its kind.
pub(super) const TARGETS_TICKET: u64 = u64::MAX - 3;

impl StudioUi {
    /// Asks what `kind` can capture; the answer fills the lists that show the kind.
    pub(super) fn list_kind_targets(&self, kind: SourceKind) {
        let Some(kind) = kind_for_api(kind) else {
            return;
        };
        let call = C::SourceTargets(rq::TargetsParams { kind });
        self.call_as(call, ApiReply::Ticket(TARGETS_TICKET));
    }

    /// The answer to [`Self::list_kind_targets`]; a refusal is on the error bar.
    pub(super) fn targets_answer(&mut self, result: Result<Response, ApiError>) {
        if let Ok(Response::Targets(list)) = result {
            let kind = kind_from_api(list.kind);
            let targets = list.targets.into_iter().map(Target::from_api).collect();
            self.targets_listed(kind, targets);
        }
    }

    /// Adds a new source of `kind` capturing `target`: with its sound, as a global
    /// source, or through the OS picker for a picker entry. The host asks the OS
    /// permission first when the kind needs it.
    pub(super) fn add_target(&self, kind: SourceKind, target: Target, audio: bool, global: bool) {
        if target.picker {
            self.send(Command::PickSource {
                kind,
                adapter: target.adapter,
            });
            return;
        }
        let Some(api_kind) = kind_for_api(kind) else {
            return;
        };
        self.call(C::SourceAdd(rq::AddSource {
            kind: api_kind,
            adapter: target.adapter,
            target: target.target,
            name: None,
            scene: None,
            global,
            with_audio: audio,
            text: None,
            stamp: None,
            path: None,
            url: None,
        }));
    }

    /// What `kind` can capture, in OS order: the Add source picker, Settings ›
    /// Global sources and the Source tab show it when they show that kind.
    fn targets_listed(&mut self, kind: SourceKind, list: Vec<Target>) {
        if let Some(p) = &mut self.add_picker
            && p.answer(kind, &list)
        {
            self.picker_dirty = true;
        }
        let picked = self.kind_list.get(self.global_kind as usize).copied();
        if picked == Some(kind) {
            self.global_targets = list.clone();
            self.refresh_global_existing();
        }
        // The Source tab's list is for the source whose kind this is.
        let for_kind = self
            .source_targets_for
            .and_then(|id| self.rows.iter().find(|r| r.id == id))
            .and_then(|r| r.kind);
        if for_kind == Some(kind) {
            self.source_target_list = list.clone();
            self.refresh_source_targets();
        }
    }
}
