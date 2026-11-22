//! Source pickers: capture targets, thumbnails, region previews and permissions.

use super::*;

impl Host {
    /// What a kind can capture, in OS order, then one entry per OS picker.
    pub(super) fn targets(&self, kind: SourceKind) -> Vec<Target> {
        let raw = self
            .adapters
            .sources
            .targets(&kind.type_id())
            .unwrap_or_default();
        // Pairing a camera with its microphone lists audio inputs; it runs only for
        // cameras, while a list is asked for (P13).
        let mics = if kind == SourceKind::Camera {
            self.unused_microphones()
        } else {
            Vec::new()
        };
        let can_exclude = self.adapters.sources.can_exclude_own_sound();
        let mut list: Vec<Target> = raw
            .into_iter()
            .map(|t| {
                let mic = t
                    .related_audio
                    .as_ref()
                    .and_then(|id| mics.iter().find(|m| &m.target == id))
                    .map(|m| m.name.clone());
                let sound = kind != SourceKind::Screen || can_exclude;
                Target {
                    mic,
                    sound,
                    ..target_of(t)
                }
            })
            .collect();
        let pickers = self
            .adapters
            .sources
            .pickers(&kind.type_id())
            .unwrap_or_default();
        list.extend(pickers.iter().map(|p| Target {
            name: picker_name(kind).to_owned(),
            adapter: p.adapter().as_str().to_owned(),
            target: String::new(),
            is_default: false,
            size: None,
            app: None,
            recent: None,
            picker: true,
            mic: None,
            sound: false,
        }));
        for t in &mut list {
            t.recent = recent_rank(self.recent.list(kind.type_id().as_str()), t);
        }
        list
    }

    /// The audio inputs that are not a source yet.
    pub(super) fn unused_microphones(&self) -> Vec<rbs_sources::SourceTarget> {
        let used: Vec<&str> = self
            .collection
            .sources()
            .filter(|s| s.type_id == SourceKind::AudioInput.type_id())
            .filter_map(|s| match s.settings.get(keys::TARGET) {
                Some(SettingValue::Text(t)) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        self.adapters
            .sources
            .targets(&SourceKind::AudioInput.type_id())
            .unwrap_or_default()
            .into_iter()
            .filter(|m| !used.contains(&m.target.as_str()))
            .collect()
    }

    /// The microphone that belongs with the camera `picture` shows, when the audio
    /// system has it and it is not a source yet.
    pub(super) fn camera_microphone(&self, picture: &rbs_scene::model::Source) -> Option<Target> {
        let (adapter, id) = match (
            picture.settings.get(keys::ADAPTER),
            picture.settings.get(keys::TARGET),
        ) {
            (Some(SettingValue::Text(a)), Some(SettingValue::Text(t))) => (a.as_str(), t.as_str()),
            _ => return None,
        };
        let related = self
            .adapters
            .sources
            .targets(&SourceKind::Camera.type_id())
            .unwrap_or_default()
            .into_iter()
            .find(|t| t.adapter.as_str() == adapter && t.target == id)?
            .related_audio?;
        let mic = self
            .unused_microphones()
            .into_iter()
            .find(|m| m.target == related)?;
        Some(target_of(mic))
    }

    /// Grabs the pictures of `rows` on up to two threads, each target once, and posts
    /// them as they come. Nothing runs when the list is closed: a newer generation
    /// ends the workers at their next row (P13).
    /// One Region-section grab for Source settings (ADR-0103). Off the control thread.
    pub(super) fn grab_region_preview(&self, id: SourceId) {
        let Some(source) = self.collection.source(id) else {
            return;
        };
        let Some(kind) = SourceKind::of(&source.type_id) else {
            return;
        };
        if !matches!(
            kind,
            SourceKind::Screen | SourceKind::Window | SourceKind::Camera
        ) {
            return;
        }
        let adapter = match source.settings.get(keys::ADAPTER) {
            Some(SettingValue::Text(text)) => text.clone(),
            _ => return,
        };
        let target = match source.settings.get(keys::TARGET) {
            Some(SettingValue::Text(text)) => text.clone(),
            _ => return,
        };
        let type_id = kind.type_id();
        let adapter_id = rbs_core::registry::AdapterId::new(adapter);
        let Some(factory) = self.adapters.sources.capture_factory(&type_id, &adapter_id) else {
            return;
        };
        let generation = self
            .pickers
            .region_grabs
            .fetch_add(1, AtomicOrdering::AcqRel)
            + 1;
        let current = Arc::clone(&self.pickers.region_grabs);
        let inbox = Weak::clone(&self.inbox);
        let _ = std::thread::Builder::new()
            .name("rbs-region-grab".into())
            .spawn(move || {
                if current.load(AtomicOrdering::Acquire) != generation {
                    return;
                }
                let picture = factory
                    .region_grab(
                        &rbs_capture::TargetId::new(target),
                        rbs_capture::REGION_GRAB_LONG,
                    )
                    .ok();
                let Some(tx) = inbox.upgrade() else {
                    return;
                };
                let _ = tx.send(Msg::RegionPreview {
                    generation,
                    id,
                    picture,
                });
            });
    }

    pub(super) fn grab_thumbnails(
        &self,
        kind: SourceKind,
        rows: Vec<(String, String)>,
        max: (u32, u32),
    ) {
        let generation = self.pickers.thumbs.fetch_add(1, AtomicOrdering::AcqRel) + 1;
        let type_id = kind.type_id();
        let size = rbs_core::media::Size {
            width: max.0,
            height: max.1,
        };
        let queue = Arc::new(Mutex::new(
            rows.into_iter()
                .filter_map(|(adapter, target)| {
                    let id = rbs_core::registry::AdapterId::new(adapter.clone());
                    let factory = self.adapters.sources.capture_factory(&type_id, &id)?;
                    Some((adapter, target, factory))
                })
                .collect::<std::collections::VecDeque<_>>(),
        ));
        for n in 0..2 {
            let (queue, current, inbox) = (
                Arc::clone(&queue),
                Arc::clone(&self.pickers.thumbs),
                Weak::clone(&self.inbox),
            );
            let spawned = std::thread::Builder::new()
                .name(format!("rbs-thumbnail-{n}"))
                .spawn(move || {
                    while current.load(AtomicOrdering::Acquire) == generation {
                        let next = queue.lock().ok().and_then(|mut q| q.pop_front());
                        let Some((adapter, target, factory)) = next else {
                            break;
                        };
                        let picture = factory
                            .thumbnail(&rbs_capture::TargetId::new(target.clone()), size)
                            .ok();
                        let Some(tx) = inbox.upgrade() else { break };
                        let _ = tx.send(Msg::Thumbnail {
                            generation,
                            adapter,
                            target,
                            picture,
                        });
                    }
                });
            if spawned.is_err() {
                break;
            }
        }
    }

    /// The permission kind to ask for before adding a source of `kind`: one that was
    /// not asked yet and can show a prompt, at most once per run (ADR-0141 decision 4).
    /// The permission a window's `source.add` of a listed target asks first, as the
    /// window's own add did (ADR-0168 decision 2). A global add never prompted.
    pub(super) fn source_prompt_first(&self, call: &rbs_api::Command) -> Option<PermissionKind> {
        match call {
            rbs_api::Command::SourceAdd(p) if !p.global && !p.target.is_empty() => {
                self.prompt_first(api_convert::app_kind(p.kind))
            }
            _ => None,
        }
    }

    /// A target the user chose in an OS picker: added as the window's own add did,
    /// with the error bar for a refusal.
    pub(super) fn add_picked(
        &mut self,
        kind: SourceKind,
        target: &Target,
        notify: &dyn Fn(Update),
    ) {
        match self.add(kind, target) {
            Ok(_) => self.apply(notify),
            Err(e) => notify(Update::Error(e.into_text())),
        }
    }

    pub(super) fn prompt_first(&self, kind: SourceKind) -> Option<PermissionKind> {
        let p = PermissionKind::of_type(&kind.type_id())?;
        if self.pickers.permission_asked.contains(&p) {
            return None;
        }
        let info = self.adapters.sources.permission(p);
        (info.status == PermissionStatus::NotAsked && info.can_prompt).then_some(p)
    }

    /// Every kind with an installed adapter and its status.
    pub(super) fn permission_rows(&self) -> Vec<(PermissionKind, PermissionInfo)> {
        PermissionKind::ALL
            .into_iter()
            .map(|k| (k, self.adapters.sources.permission(k)))
            .filter(|(_, info)| info.status != PermissionStatus::Unavailable)
            .collect()
    }

    pub(super) fn send_permissions(&self, notify: &dyn Fn(Update)) {
        notify(Update::Permissions {
            rows: self.permission_rows(),
            waiting: self.pickers.permission_asking.keys().next().copied(),
        });
    }

    /// Shows the OS prompt for `kind` on a one-shot worker; `then` runs when it is
    /// answered. A second request while one shows waits for the same answer.
    pub(super) fn ask_permission(
        &mut self,
        kind: PermissionKind,
        then: Option<Command>,
        notify: &dyn Fn(Update),
    ) {
        if let Some(waiting) = self.pickers.permission_asking.get_mut(&kind) {
            waiting.extend(then);
            return;
        }
        if !self.pickers.permission_asked.contains(&kind) {
            self.pickers.permission_asked.push(kind);
        }
        let request = self.adapters.sources.permission_request(kind);
        if request.is_empty() {
            if let Some(c) = then {
                self.post(Msg::Ui(c));
            }
            self.send_permissions(notify);
            return;
        }
        self.pickers
            .permission_asking
            .insert(kind, then.into_iter().collect());
        self.send_permissions(notify);
        let inbox = Weak::clone(&self.inbox);
        // Blocks until the user answers; detached, so quitting never waits for it.
        let spawned = std::thread::Builder::new()
            .name("rbs-permission".into())
            .spawn(move || {
                request.run();
                if let Some(tx) = inbox.upgrade() {
                    let _ = tx.send(Msg::PermissionAnswered(kind));
                }
            });
        if spawned.is_err() {
            self.post(Msg::PermissionAnswered(kind));
        }
    }

    /// A prompt was answered: run what waited for it, reopen sources of the kind that
    /// were refused when it is allowed now (not for kinds that apply after a restart),
    /// and tell the UI.
    pub(super) fn permission_answered(&mut self, kind: PermissionKind, notify: &dyn Fn(Update)) {
        let waiting = self
            .pickers
            .permission_asking
            .remove(&kind)
            .unwrap_or_default();
        let info = self.adapters.sources.permission(kind);
        if info.status == PermissionStatus::Allowed && !info.restart {
            let refused: Vec<SourceId> = self
                .studio
                .sources()
                .iter()
                .filter(|st| st.fault == Some(SourceFault::PermissionDenied))
                .map(|st| st.source)
                .filter(|id| {
                    self.collection
                        .source(*id)
                        .and_then(|s| PermissionKind::of_type(&s.type_id))
                        == Some(kind)
                })
                .collect();
            let reopen = !refused.is_empty();
            for id in refused {
                let _ = self.reload_source(id);
            }
            if reopen {
                self.apply(notify);
            }
        }
        for c in waiting {
            self.post(Msg::Ui(c));
        }
        self.send_permissions(notify);
    }

    /// Shows the OS picker of `entry.adapter` on a thread of its own (it blocks while
    /// the dialog is up, and this thread keeps the studio and any recording running),
    /// then adds the chosen target like a listed one. Closing the dialog adds nothing.
    pub(super) fn pick(&self, kind: SourceKind, adapter: &str) {
        let Some(picker) = self
            .adapters
            .sources
            .pickers(&kind.type_id())
            .unwrap_or_default()
            .into_iter()
            .find(|p| p.adapter().as_str() == adapter)
        else {
            // The entry's name, as the list showed it.
            let name = self
                .targets(kind)
                .into_iter()
                .find(|t| t.picker && t.adapter == adapter)
                .map_or_else(|| format!("The {} picker", kind_noun(kind)), |t| t.name);
            self.post(Msg::PickFailed(format!(
                "{name} is not available any more."
            )));
            return;
        };
        let inbox = Weak::clone(&self.inbox);
        let spawned = std::thread::Builder::new()
            .name("rbs-picker".into())
            .spawn(move || {
                let msg = match picker.pick() {
                    Ok(t) => Msg::Picked {
                        kind,
                        target: target_of(t),
                    },
                    Err(SourceError::Capture(CaptureError::Cancelled)) => return,
                    Err(e) => {
                        Msg::PickFailed(format!("The {} could not be chosen: {e}", kind_noun(kind)))
                    }
                };
                if let Some(tx) = inbox.upgrade() {
                    let _ = tx.send(msg);
                }
            });
        if let Err(e) = spawned {
            self.post(Msg::PickFailed(format!("The picker could not start: {e}")));
        }
    }
}

/// The source pickers' state: thumbnail and region grabs, and the permission prompts.
#[derive(Default)]
pub(super) struct Pickers {
    /// The generation of the thumbnail request that may still deliver (ADR-0077).
    pub(super) thumbs: Arc<AtomicU64>,
    /// OS permission prompts showing now, each with the commands waiting for its
    /// answer (ADR-0141 decision 4). One prompt per kind at a time.
    pub(super) permission_asking: HashMap<PermissionKind, Vec<Command>>,
    /// Kinds asked for in this run: adding a source prompts at most once per kind.
    pub(super) permission_asked: Vec<PermissionKind>,
    /// The kinds the window needs a status for, when it last got the statuses: kinds
    /// some shown source was refused, and Input monitoring while a chain has a pointer
    /// filter (ADR-0165 decision 7). A new kind sends the statuses, so Properties
    /// offers Allow or Fix permission.
    pub(super) permission_refused: Vec<PermissionKind>,
    /// The generation of the region-preview grab that may still deliver (ADR-0103).
    pub(super) region_grabs: Arc<AtomicU64>,
}
