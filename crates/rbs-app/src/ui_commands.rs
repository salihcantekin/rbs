//! External registry calls never enter the local menu's engine dispatch path.

use std::cell::{Cell, RefCell};

use rbs_api::ui::{
    UiCommandInfo, UiForwarded, UiRequest, UiResolution, UiResponder, UiRouter, authorize_engine,
    authorize_method, authorize_ui,
};
use rbs_api::{ApiError, ErrorKind, Response};
use rbs_ui::command::{CommandId, CommandState, Kind};
use serde_json::json;

use super::{App, CTX, menu_bar, studio_ui};
use menu_bar::api::{Resolution, call};

#[cfg(test)]
mod tests;

thread_local! {
    static PICKER: RefCell<Option<PendingPicker>> = const { RefCell::new(None) };
    static FILE_PICKER: Cell<bool> = const { Cell::new(false) };
}

struct FilePicker;

impl Drop for FilePicker {
    fn drop(&mut self) {
        FILE_PICKER.with(|open| open.set(false));
    }
}

struct PendingPicker {
    call: UiForwarded,
    reply: UiResponder,
    scene: i32,
}

struct Delivery {
    call: UiForwarded,
    reply: UiResponder,
    delivered: bool,
}

impl Delivery {
    fn deliver(mut self, app: &mut App) {
        dispatch(app, self.call.clone(), self.reply.clone());
        self.delivered = true;
    }

    fn complete(mut self) {
        self.delivered = true;
    }
}

impl Drop for Delivery {
    fn drop(&mut self) {
        if !self.delivered {
            self.reply
                .send(self.call.resolve(UiResolution::Complete(Err(ApiError::new(
                    ErrorKind::Failed,
                    "the UI command was interrupted before it could answer",
                )))));
        }
    }
}

impl App {
    pub(super) fn close_picker_ui(&mut self) {
        cancel_picker();
        self.studio.close_add_picker();
        self.with_picker_overlays(|o| o.set_picker_open(false));
        self.picker_host = None;
    }
}

pub(super) fn chosen(app: &mut App, index: i32) -> bool {
    usize::try_from(index).is_ok_and(|index| {
        choose_picker(app, index).unwrap_or_else(|| app.studio.choose_add_picker(index))
    })
}

pub(super) fn register(router: UiRouter) -> Result<(), ApiError> {
    router.register("desktop".into(), "RBS desktop".into(), |call, reply| {
        let delivery = Delivery {
            call,
            reply,
            delivered: false,
        };
        if let Err(error) = super::entry::invoke(move || {
            let ctx = CTX.with(|ctx| ctx.borrow().clone());
            if let Some(ctx) = ctx {
                ctx.update(move |app| delivery.deliver(app));
            }
        }) {
            rbs_core::warn!(
                rbs_core::log::Area::Remote,
                "UI event loop unavailable: {error}"
            );
        }
    })?;
    router.focus("desktop")
}

fn dispatch(app: &mut App, call: UiForwarded, reply: UiResponder) {
    if !call.ticket.is_active() {
        reply.send(call.resolve(UiResolution::Complete(Err(ApiError::new(
            ErrorKind::Conflict,
            "the UI request was cancelled",
        )))));
        return;
    }
    let result = match &call.request {
        UiRequest::List { .. } => Ok(Response::UiCommands(list(app, &call.caller))),
        UiRequest::Run { id, .. } => {
            let resolution = check_method(app, id, &call.caller)
                .and_then(|()| menu_bar::api::resolve(id, &app.shown()));
            match resolution {
                Ok(Resolution::Engine(command)) => {
                    reply.send(call.resolve(UiResolution::Engine(command)));
                    return;
                }
                Ok(Resolution::Batch(commands)) => {
                    if commands.is_empty() {
                        Ok(Response::Done(rbs_api::request::Empty {}))
                    } else {
                        reply.send(call.resolve(UiResolution::Batch(commands)));
                        return;
                    }
                }
                Ok(Resolution::Picker(what)) => {
                    if let Err(error) = authorize_method(&call.caller, "source.add") {
                        Err(error)
                    } else {
                        open_picker(app, call, reply, what);
                        return;
                    }
                }
                Ok(Resolution::Ui(run)) => run_ui(app, id, run, &call.caller),
                Err(error) => Err(error),
            }
        }
    };
    reply.send(call.resolve(UiResolution::Complete(result)));
}

fn method(entry: &rbs_ui::command::Entry, checked: bool) -> &'static str {
    match entry.kind {
        Kind::Action { method } | Kind::Picker { method, .. } => method,
        Kind::Toggle { on, off } | Kind::ResolvedToggle { on, off } => {
            if checked {
                off
            } else {
                on
            }
        }
    }
}

fn check_method(app: &App, text: &str, caller: &rbs_api::Caller) -> Result<(), ApiError> {
    let id = CommandId::parse(text)
        .map_err(|error| ApiError::new(ErrorKind::InvalidParams, error.to_string()))?;
    let entry = id
        .resolve()
        .map_err(|error| ApiError::new(ErrorKind::InvalidParams, error.to_string()))?
        .ok_or_else(|| ApiError::new(ErrorKind::NotFound, "unknown registry id"))?;
    let method = method(entry, app.shown().checked(&id));
    authorize_method(caller, method)?;
    supported(method)
}

fn supported(method: &str) -> Result<(), ApiError> {
    if ["ui.profile.", "ui.collection."].iter().any(|prefix| {
        method.strip_prefix(prefix).is_some_and(|op| {
            matches!(
                op,
                "new" | "duplicate" | "rename" | "delete" | "import" | "export"
            )
        })
    }) {
        return Err(ApiError::new(
            ErrorKind::NotPermitted,
            "library changes require a caller-preserving writer path",
        ));
    }
    Ok(())
}

fn list(app: &App, caller: &rbs_api::Caller) -> Vec<UiCommandInfo> {
    let shown = app.shown();
    rbs_ui::command::ENTRIES
        .iter()
        .map(|entry| {
            let id = CommandId::plain(entry.id);
            let checked = id.as_ref().is_ok_and(|id| shown.checked(id));
            let method = method(entry, checked);
            let permission = authorize_method(caller, method)
                .and_then(|()| supported(method))
                .and_then(|()| {
                    if rbs_api::request::UI_METHODS.contains(&method) {
                        authorize_ui(caller, method)
                    } else if !caller.irreversible
                        && rbs_api::request::method_effect(method)
                            .is_some_and(|effect| effect.is_irreversible())
                    {
                        Err(ApiError::new(
                            ErrorKind::NotPermitted,
                            "this action cannot be undone",
                        ))
                    } else {
                        Ok(())
                    }
                });
            let reason = permission.err().map(|error| error.message).or_else(|| {
                id.as_ref()
                    .ok()
                    .and_then(|id| shown.enabled(id).err())
                    .map(|reason| reason.to_string())
            });
            UiCommandInfo {
                id: entry.id.into(),
                title: entry.title.into(),
                kind: match entry.kind {
                    Kind::Action { .. } => "action",
                    Kind::Picker { .. } => "picker",
                    Kind::Toggle { .. } | Kind::ResolvedToggle { .. } => "toggle",
                }
                .into(),
                enabled: reason.is_none(),
                reason,
                checked: entry.checkable.then_some(checked),
            }
        })
        .collect()
}

fn run_ui(
    app: &mut App,
    text: &str,
    run: menu_bar::Run,
    caller: &rbs_api::Caller,
) -> Result<Response, ApiError> {
    let id = CommandId::parse(text)
        .map_err(|error| ApiError::new(ErrorKind::InvalidParams, error.to_string()))?;
    let entry = id
        .resolve()
        .map_err(|error| ApiError::new(ErrorKind::InvalidParams, error.to_string()))?
        .ok_or_else(|| ApiError::new(ErrorKind::NotFound, "unknown registry id"))?;
    authorize_ui(caller, method(entry, app.shown().checked(&id)))?;
    match run {
        menu_bar::Run::SaveWorkspaceAs(Some(name)) => {
            app.save_workspace_as(&name)
                .map_err(|error| ApiError::new(ErrorKind::Conflict, error))?;
        }
        menu_bar::Run::Entry(_, menu_bar::EntryOp::Import | menu_bar::EntryOp::Export, _)
        | menu_bar::Run::Entry(
            _,
            menu_bar::EntryOp::New | menu_bar::EntryOp::Duplicate | menu_bar::EntryOp::Rename,
            None,
        ) => {
            return Err(ApiError::new(
                ErrorKind::InvalidParams,
                "this command needs input at the window",
            ));
        }
        menu_bar::Run::Entry(..) => {
            return Err(ApiError::new(
                ErrorKind::NotPermitted,
                "library changes require a caller-preserving writer path",
            ));
        }
        menu_bar::Run::ReloadThemes
        | menu_bar::Run::SelectTheme(_)
        | menu_bar::Run::SelectExperience(_)
        | menu_bar::Run::SelectDensity(_)
        | menu_bar::Run::OpenSettings(..)
        | menu_bar::Run::OpenAppMenu
        | menu_bar::Run::Window(_)
        | menu_bar::Run::OpenObsImport
        | menu_bar::Run::OpenGuest(_)
        | menu_bar::Run::Workspace(_)
        | menu_bar::Run::Panel(_)
        | menu_bar::Run::Layout(_)
        | menu_bar::Run::Maximize(..)
        | menu_bar::Run::SaveWorkspace
        | menu_bar::Run::LayoutHistory(_)
        | menu_bar::Run::AutoHide(_)
        | menu_bar::Run::MovePanel(..)
        | menu_bar::Run::OpenSourceSettings(_)
        | menu_bar::Run::ShowProperties { .. }
        | menu_bar::Run::MixerOrientation(_)
        | menu_bar::Run::Projector(..)
        | menu_bar::Run::ProjectorSize(..) => app.run_command_as(text, false),
        _ => {
            return Err(ApiError::new(
                ErrorKind::MethodNotFound,
                "this operation has no external UI resolver",
            ));
        }
    }
    Ok(Response::Done(rbs_api::request::Empty {}))
}

fn open_picker(app: &mut App, call: UiForwarded, reply: UiResponder, what: menu_bar::AddWhat) {
    if app.studio.add_picker.is_some()
        || PICKER.with(|picker| picker.borrow().is_some())
        || FILE_PICKER.with(Cell::get)
    {
        reply.send(call.resolve(UiResolution::Complete(Err(ApiError::new(
            ErrorKind::Conflict,
            "an Add source picker is already open",
        )))));
        return;
    }
    let ticket = call.ticket.clone();
    if let Err(error) = call.ticket.observe_cancellation(move || {
        let _ = super::entry::invoke(move || {
            if let Some(ctx) = CTX.with(|ctx| ctx.borrow().clone()) {
                ctx.update(move |app| {
                    if PICKER.with(|picker| {
                        picker
                            .borrow()
                            .as_ref()
                            .is_some_and(|pending| pending.call.ticket == ticket)
                    }) {
                        app.close_picker_ui();
                    }
                });
            }
        });
    }) {
        reply.send(call.resolve(UiResolution::Complete(Err(error))));
        return;
    }
    if !call.ticket.is_active() {
        return;
    }
    app.close_picker_ui();
    let Some(scene) = app.studio.scene_target(None) else {
        reply.send(call.resolve(UiResolution::Complete(Err(ApiError::new(
            ErrorKind::Conflict,
            "the studio has no scene to add the source to",
        )))));
        return;
    };
    let kind = match what {
        menu_bar::AddWhat::Kind(code) => app.studio.kind_at(code),
        menu_bar::AddWhat::Existing => None,
    };
    let pending = PendingPicker { call, reply, scene };
    use super::studio_host::SourceKind as K;
    match kind {
        Some(K::Color) => {
            finish_made(
                pending,
                json!({"kind": "color", "stamp": "rectangle", "scene": scene}),
            );
            return;
        }
        Some(K::Text) => {
            finish_made(
                pending,
                json!({"kind": "text", "text": "Text", "scene": scene}),
            );
            return;
        }
        Some(K::Browser) => {
            finish_made(
                pending,
                json!({"kind": "browser", "url": "", "scene": scene}),
            );
            return;
        }
        Some(K::Image | K::Media | K::Slideshow) => {
            pick_file(pending, kind);
            return;
        }
        Some(_) => {
            if let Err(error) = authorize_method(&pending.call.caller, "source.targets") {
                pending
                    .reply
                    .send(pending.call.resolve(UiResolution::Complete(Err(error))));
                return;
            }
        }
        None => {}
    }
    PICKER.with(|picker| *picker.borrow_mut() = Some(pending));
    app.picker_host = None;
    app.studio.open_add_picker(what);
    app.sync_picker();
}

pub(super) fn cancel_picker() {
    if let Some(pending) = PICKER.with(|picker| picker.borrow_mut().take()) {
        pending.reply.send(
            pending
                .call
                .resolve(UiResolution::Complete(Err(ApiError::new(
                    ErrorKind::Conflict,
                    "the picker was cancelled",
                )))),
        );
    }
}

pub(super) fn choose_picker(app: &mut App, index: usize) -> Option<bool> {
    let pending = PICKER.with(|picker| picker.borrow_mut().take())?;
    if !pending.call.ticket.is_active() {
        return Some(true);
    }
    let pick = app
        .studio
        .add_picker
        .as_ref()
        .and_then(|picker| picker.pick(index))
        .cloned();
    let audio = app
        .studio
        .add_picker
        .as_ref()
        .is_some_and(|picker| picker.audio());
    let command = picker_command(pick, pending.scene, audio);
    match command {
        Ok(command) => match authorize_engine(&pending.call.caller, &command) {
            Ok(()) => pending
                .reply
                .send(pending.call.resolve(UiResolution::Engine(command))),
            Err(error) => pending
                .reply
                .send(pending.call.resolve(UiResolution::Complete(Err(error)))),
        },
        Err(error) => pending
            .reply
            .send(pending.call.resolve(UiResolution::Complete(Err(error)))),
    }
    Some(true)
}

fn picker_command(
    pick: Option<studio_ui::add_picker::Pick>,
    scene: i32,
    audio: bool,
) -> Result<rbs_api::Command, ApiError> {
    match pick {
        Some(studio_ui::add_picker::Pick::Target(kind, target)) if !target.picker => {
            let audio =
                audio && (kind != super::studio_host::SourceKind::Camera || target.mic.is_some());
            call(
                "source.add",
                json!({
                    "kind": kind.api_id(), "adapter": target.adapter, "target": target.target,
                    "scene": scene, "with_audio": audio,
                }),
            )
        }
        Some(studio_ui::add_picker::Pick::Existing(source)) => call(
            "item.add",
            json!({
                "scene": scene, "source": source.get(),
            }),
        ),
        Some(studio_ui::add_picker::Pick::Target(_, target)) if target.picker => {
            let mut data = serde_json::Map::new();
            data.insert("reason".into(), "local_only".into());
            Err(ApiError::with_data(
                ErrorKind::NotPermitted,
                "native capture pickers have no caller-preserving routing path",
                data,
            ))
        }
        _ => Err(ApiError::new(
            ErrorKind::InvalidParams,
            "choose a listed capture target",
        )),
    }
}

fn finish_made(pending: PendingPicker, params: serde_json::Value) {
    if !pending.call.ticket.is_active() {
        return;
    }
    let resolution = match call("source.add", params) {
        Ok(command) => UiResolution::Engine(command),
        Err(error) => UiResolution::Complete(Err(error)),
    };
    pending.reply.send(pending.call.resolve(resolution));
}

fn pick_file(pending: PendingPicker, kind: Option<super::studio_host::SourceKind>) {
    use super::{entry, file_dialog, studio_host::SourceKind as K};
    let ticket = pending.call.ticket.clone();
    FILE_PICKER.with(|open| open.set(true));
    let open = FilePicker;
    let delivery = Delivery {
        call: pending.call.clone(),
        reply: pending.reply.clone(),
        delivered: false,
    };
    PICKER.with(|picker| *picker.borrow_mut() = Some(pending));
    let task = entry::spawn_local(async move {
        let picked = match kind {
            Some(K::Image) => file_dialog::open_png("Choose a PNG image").await,
            Some(K::Media) => file_dialog::open_media("Choose a media file").await,
            _ => file_dialog::open_folder("Choose a folder of pictures").await,
        };
        drop(open);
        let pending = PICKER.with(|picker| {
            let mut picker = picker.borrow_mut();
            if picker
                .as_ref()
                .is_some_and(|pending| pending.call.ticket == ticket)
            {
                picker.take()
            } else {
                None
            }
        });
        let Some(pending) = pending else { return };
        match picked {
            Ok(Some(path)) => {
                let params = json!({
                    "kind": kind.map(|kind| kind.api_id()), "scene": pending.scene,
                    "path": path.to_string_lossy(),
                });
                finish_made(pending, params);
            }
            other => {
                let error = match other {
                    Err(error) => ApiError::new(ErrorKind::Failed, error),
                    _ => ApiError::new(ErrorKind::Conflict, "the picker was cancelled"),
                };
                pending
                    .reply
                    .send(pending.call.resolve(UiResolution::Complete(Err(error))));
            }
        }
        delivery.complete();
    });
    if let Err(error) = task
        && let Some(pending) = PICKER.with(|picker| picker.borrow_mut().take())
    {
        pending.reply.send(
            pending
                .call
                .resolve(UiResolution::Complete(Err(ApiError::new(
                    ErrorKind::Failed,
                    error.to_string(),
                )))),
        );
    }
}
