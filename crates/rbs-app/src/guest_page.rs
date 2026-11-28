//! The guest window (ADR-0139 phase 1d): Share (this RBS sends its program) and Add
//! guest (this RBS receives one). The pages and the calls are in `rbs_app::guest_view`;
//! this file owns the window, sends the session's calls to the studio as control-API
//! commands (`guest.*`, protocol 1.116) and hands the answers back.
//!
//! A code is a secret: the one shown and the one pasted are registered with
//! `rbs_crash::add_secret` while they live, and no call is ever printed with its text.

use std::cell::{Cell, RefCell};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rbs_api::request::{GuestAcceptAnswer, GuestCommand, GuestShare, GuestShareRef, GuestState};
use rbs_api::{ApiError, Command as Api, Response};
use rbs_app::guest_settings::{self, GuestSettings};
use rbs_app::guest_view::{self, Call, CallError, Flow, Reply, Row, Session, State, Status, words};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use crate::ui::{GuestWindow, SettingsWindow, Theme, Tokens};
use crate::{App, Ctx, entry, look, window_place};

thread_local! {
    /// Calls sent and not answered yet, by ticket.
    static PENDING: RefCell<Vec<(u64, Call)>> = const { RefCell::new(Vec::new()) };
    /// Answers that came back, for the next pump.
    static DONE: RefCell<Vec<(Call, Result<Response, ApiError>)>> = const { RefCell::new(Vec::new()) };
    /// The ticket of the Settings page's status question, while it is out.
    static SETTINGS_ASKED: Cell<Option<u64>> = const { Cell::new(None) };
    /// The shares the last status answer listed, for the Settings page.
    static SHARES: RefCell<Vec<Line>> = const { RefCell::new(Vec::new()) };
}

/// One share of a status answer, as Settings › Guests shows it.
struct Line {
    share: u32,
    state: State,
    text: String,
}

/// How often the open window asks for the pairing's state while it is open.
const POLL: Duration = Duration::from_millis(250);

/// The open window and its session.
pub struct GuestPage {
    pub window: GuestWindow,
    session: Session,
    /// The code registered as a secret while it is shown.
    secret: String,
}

/// Opens the window for `sharing`, or brings it forward when one is open.
pub fn open(ctx: &Ctx, app: &mut App, sharing: bool) {
    let flow = if sharing { Flow::Share } else { Flow::Host };
    if let Some(page) = &app.guest {
        if page.session.flow() == flow {
            window_place::raise(page.window.window());
            return;
        }
        // The other flow was asked for: end the pairing that is still being made and
        // open the new page.
        close(app);
    }
    let Ok(w) = GuestWindow::new() else {
        app.show_error(crate::UiError::Other(
            "The guest window cannot be opened.".into(),
        ));
        return;
    };
    rbs_app::window_events::install(&w);
    look::apply(
        &w.global::<Theme>(),
        &w.global::<Tokens>(),
        ctx.look.borrow().as_ref(),
    );
    let c = ctx.clone();
    entry::on_close_requested(w.window(), move || {
        c.update(close);
        slint::CloseRequestResponse::HideWindow
    });
    install(&w, ctx);
    PENDING.with(|p| p.borrow_mut().clear());
    DONE.with(|d| d.borrow_mut().clear());
    let mut page = GuestPage {
        window: w,
        session: Session::new(flow),
        secret: String::new(),
    };
    page.session.start();
    app.guest = Some(page);
    pump(app);
    let Some(page) = app.guest.as_ref() else {
        return;
    };
    let size = slint::LogicalSize::new(
        page.window.global::<Tokens>().get_size_guest_w(),
        page.window.get_wanted_height(),
    );
    let _ = window_place::show_centred(&page.window, &app.main, size, app.hidden);
    let _ = crate::exclude_window_from_capture(page.window.window(), app.bg.exclude_own_window);
    app.capture_applied.1 = usize::MAX;
    app.capture_retry = None;
    schedule(ctx);
}

/// Hides the window and ends a pairing that has not gone live.
fn close(app: &mut App) {
    if let Some(mut page) = app.guest.take() {
        // A running share outlives the window; one still pairing does not.
        if page.session.status().state != State::Live {
            page.session.stop();
        }
        for call in page.session.take_calls() {
            send(app, &call);
        }
        forget(&mut page.secret);
        let _ = page.window.hide();
    }
}

/// Removes a registered secret.
fn forget(secret: &mut String) {
    if !secret.is_empty() {
        rbs_crash::remove_secret(secret);
        secret.clear();
    }
}

/// Asks the studio again after [`POLL`], for as long as the window is open: with no
/// pairing, or one that ended, nothing is asked.
fn schedule(ctx: &Ctx) {
    let c = ctx.clone();
    entry::single_shot(POLL, move || {
        let again = c.clone();
        c.update(move |app| {
            if app.guest.is_none() {
                return;
            }
            pump(app);
            schedule(&again);
        });
    });
}

/// The control-API command of a call.
fn command(call: &Call, settings: &GuestSettings) -> Option<Api> {
    let share = |share| GuestShareRef { share };
    Some(Api::Guest(match call {
        Call::Share => GuestCommand::Share(GuestShare {
            lifetime_s: Some(settings.lifetime_s()),
            check_phrase: Some(settings.check_phrase),
            local_only: Some(settings.local_only),
            ..GuestShare::default()
        }),
        Call::Code(id) => GuestCommand::ShareCode(share(*id)),
        Call::Answer(id, text) => GuestCommand::AcceptAnswer(GuestAcceptAnswer {
            share: *id,
            answer: text.clone(),
        }),
        Call::Confirm(id) => GuestCommand::Confirm(share(*id)),
        Call::Stop(id) => GuestCommand::Stop(share(*id)),
        Call::Status => GuestCommand::Status,
        // The host side comes with `guest.source.*` and the guest source.
        Call::AddGuest(_) => return None,
    }))
}

/// Sends a call to the studio and remembers its ticket; a call the studio cannot take
/// is answered at once.
fn send(app: &App, call: &Call) {
    if let Call::Answer(_, text) | Call::AddGuest(text) = call {
        rbs_crash::add_secret(text);
    }
    let settings = guest_settings::load(&app.config).0;
    let ticket = match (command(call, &settings), app.studio.host.is_some()) {
        (Some(c), true) => app.studio.ask_quietly(c),
        (command, host) => {
            let why = if command.is_none() {
                "Adding a guest needs a later version of RBS."
            } else if host {
                "This call is not available."
            } else {
                "The studio is not running."
            };
            DONE.with(|d| {
                d.borrow_mut().push((
                    call.clone(),
                    Err(ApiError::new(rbs_api::ErrorKind::Conflict, why)),
                ))
            });
            return;
        }
    };
    PENDING.with(|p| p.borrow_mut().push((ticket, call.clone())));
}

/// The studio's answer to a question, called with every answer the window gets
/// (`Update::ApiAnswer`): one of this window's is kept for the next pump and `None`
/// comes back; any other is returned.
pub(crate) fn answered(
    ticket: u64,
    result: Result<Response, ApiError>,
) -> Option<Result<Response, ApiError>> {
    if SETTINGS_ASKED.get() == Some(ticket) {
        SETTINGS_ASKED.set(None);
        if let Ok(Response::GuestStatus(s)) = &result {
            let lines = s
                .data
                .shares
                .iter()
                .map(|r| Line {
                    share: r.share,
                    state: state(r.state),
                    text: format!(
                        "Share {} · {}×{} · {} kbit/s",
                        r.share, r.width, r.height, r.bitrate_kbps
                    ),
                })
                .collect();
            SHARES.with(|l| *l.borrow_mut() = lines);
        }
        return None;
    }
    let call = PENDING.with(|p| {
        let mut p = p.borrow_mut();
        let at = p.iter().position(|(t, _)| *t == ticket)?;
        Some(p.remove(at).1)
    });
    match call {
        Some(call) => {
            DONE.with(|d| d.borrow_mut().push((call, result)));
            None
        }
        None => Some(result),
    }
}

fn state(s: GuestState) -> State {
    match s {
        GuestState::Gathering => State::Gathering,
        GuestState::WaitingForAnswer => State::WaitingForAnswer,
        GuestState::Connecting => State::Connecting,
        GuestState::CheckPhrase => State::CheckPhrase,
        GuestState::Live => State::Live,
        GuestState::Failed => State::Failed,
        GuestState::Closed | GuestState::Other => State::Closed,
    }
}

/// An answer as the session reads it: the `guest_*` reason of a refusal when there is
/// one, else its text.
fn reply(result: Result<Response, ApiError>) -> Result<Reply, CallError> {
    match result {
        Err(e) => Err(CallError(
            e.data
                .as_ref()
                .and_then(|d| d.get("reason"))
                .and_then(|r| r.as_str())
                .map_or(e.message, str::to_string),
        )),
        Ok(Response::GuestShareStarted(s)) => Ok(Reply::Started(s.share, state(s.state))),
        Ok(Response::GuestCode(c)) => Ok(Reply::Code(c.code)),
        Ok(Response::GuestShareState(s)) => Ok(Reply::State(state(s.state))),
        Ok(Response::GuestStatus(s)) => Ok(Reply::Rows(
            s.data
                .shares
                .into_iter()
                .map(|r| Row {
                    share: r.share,
                    state: state(r.state),
                    phrase: r.phrase.as_deref().map(words).unwrap_or_default(),
                    reason: r.reason,
                    expires_at: r.expires_at,
                })
                .collect(),
        )),
        Ok(_) => Ok(Reply::Done),
    }
}

/// Takes the answers that came back, asks what the session wants asked, and shows
/// the result.
fn pump(app: &mut App) {
    let Some(mut page) = app.guest.take() else {
        return;
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    for (call, result) in DONE.with(|d| std::mem::take(&mut *d.borrow_mut())) {
        if let Call::Answer(_, text) | Call::AddGuest(text) = &call {
            rbs_crash::remove_secret(text);
        }
        page.session.answer(&call, reply(result), now);
    }
    page.session.tick();
    for call in page.session.take_calls() {
        send(app, &call);
    }
    page.push();
    app.guest = Some(page);
}

impl GuestPage {
    /// Pushes what the session shows into the window.
    fn push(&mut self) {
        // The code is a secret while it is shown (ADR-0139 decision 14).
        if self.session.code() != self.secret {
            forget(&mut self.secret);
            self.secret = self.session.code().to_string();
            if !self.secret.is_empty() {
                rbs_crash::add_secret(&self.secret);
            }
        }
        let w = &self.window;
        let v = self.session.view();
        let words = &self.session.status().phrase;
        let strings = |s: &[String]| {
            ModelRc::new(VecModel::from(
                s.iter()
                    .map(|x| SharedString::from(x.as_str()))
                    .collect::<Vec<_>>(),
            ))
        };
        w.set_sharing(self.session.flow() == Flow::Share);
        w.set_page(v.page.index());
        w.set_heading(v.title.as_str().into());
        w.set_hint(v.hint.as_str().into());
        let marks: Vec<i32> = v.steps.iter().map(|m| m.index()).collect();
        w.set_steps(ModelRc::new(VecModel::from(marks)));
        w.set_badge(v.badge.as_str().into());
        w.set_tone(v.tone);
        w.set_expires(v.expires.as_str().into());
        w.set_failure(v.failure.as_str().into());
        w.set_code(self.session.code().into());
        w.set_code_ready(v.code_ready && !self.session.code().is_empty());
        w.set_paste_enabled(v.paste_enabled);
        let split = words.len().min(3);
        w.set_words_top(strings(&words[..split]));
        w.set_words_bottom(strings(&words[split..]));
        w.set_problem(self.session.problem().into());
    }
}

fn install(w: &GuestWindow, ctx: &Ctx) {
    macro_rules! on {
        ($name:ident, |$page:ident $(, $arg:ident : $ty:ty)*| $body:block) => {{
            let ctx = ctx.clone();
            w.$name(move |$($arg: $ty),*| {
                ctx.update(move |app: &mut App| {
                    if let Some($page) = app.guest.as_mut() $body
                    pump(app);
                })
            });
        }};
    }
    on!(on_submit, |page, text: SharedString| {
        page.session.submit(text.as_str());
        page.window.set_pasted(SharedString::new());
    });
    on!(on_confirm, |page| {
        page.session.confirm();
    });
    on!(on_mismatch, |page| {
        page.session.stop();
    });
    on!(on_stop, |page| {
        page.session.stop();
    });
    on!(on_again, |page| {
        page.session.again();
    });
    {
        let c = ctx.clone();
        w.on_dismiss(move || {
            entry::guard(|| c.update(close));
        });
    }
}

// ---- Settings › Guests --------------------------------------------------------

/// The Settings page's callbacks (the page is `ui/guests.slint`).
pub(crate) fn install_settings(w: &SettingsWindow, ctx: &Ctx) {
    macro_rules! on {
        ($name:ident, |$app:ident| $body:block) => {{
            let ctx = ctx.clone();
            w.$name(move || {
                entry::guard(|| {
                    ctx.update(move |$app: &mut App| $body);
                });
            });
        }};
    }
    on!(on_guest_shown, |app| {
        let (settings, note) = guest_settings::load(&app.config);
        if let Some(note) = note {
            app.show_error(crate::UiError::Other(note));
        }
        if let Some(w) = &app.settings {
            w.set_guest_check_phrase(settings.check_phrase);
            w.set_guest_lifetime(settings.lifetime_index() as i32);
            w.set_guest_local_only(settings.local_only);
            let labels: Vec<SharedString> = guest_settings::lifetime_labels()
                .into_iter()
                .map(Into::into)
                .collect();
            w.set_guest_lifetimes(ModelRc::new(VecModel::from(labels)));
            w.set_guest_add_text("Adding a guest needs a later version of RBS.".into());
        }
    });
    on!(on_guest_settings_changed, |app| {
        let Some(w) = &app.settings else { return };
        let settings = GuestSettings {
            check_phrase: w.get_guest_check_phrase(),
            lifetime_min: usize::try_from(w.get_guest_lifetime())
                .ok()
                .and_then(|i| guest_settings::LIFETIMES.get(i))
                .copied()
                .unwrap_or(GuestSettings::default().lifetime_min),
            local_only: w.get_guest_local_only(),
            ..GuestSettings::default()
        };
        if let Err(e) = guest_settings::save(&app.config, &settings) {
            app.show_error(crate::UiError::Other(format!("guest.toml: {e}")));
        }
    });
    on!(on_guest_refresh, |app| {
        push_shares(app);
        if app.studio.host.is_some() && SETTINGS_ASKED.get().is_none() {
            let ticket = app.studio.ask_quietly(Api::Guest(GuestCommand::Status));
            SETTINGS_ASKED.set(Some(ticket));
        }
    });
    {
        let c = ctx.clone();
        w.on_guest_open_share(move || {
            entry::guard(|| {
                let ctx = c.clone();
                c.update(move |app| open(&ctx, app, true));
            });
        });
    }
    on!(on_guest_stop_share, |app| {
        let ids: Vec<u32> = SHARES.with(|l| {
            l.borrow()
                .iter()
                .filter(|l| !matches!(l.state, State::Failed | State::Closed))
                .map(|l| l.share)
                .collect()
        });
        for id in ids {
            send(app, &Call::Stop(id));
        }
        // Gone at once; the next status answer says what is left.
        SHARES.with(|l| l.borrow_mut().clear());
        push_shares(app);
    });
}

/// Shows the running share, if any, on the Settings page.
fn push_shares(app: &App) {
    let Some(w) = &app.settings else { return };
    SHARES.with(|l| {
        let l = l.borrow();
        let active = l
            .iter()
            .find(|l| !matches!(l.state, State::Failed | State::Closed));
        w.set_guest_sharing(active.is_some());
        if let Some(line) = active {
            let v = guest_view::view(
                Flow::Share,
                &Status {
                    state: line.state,
                    phrase: Vec::new(),
                    reason: None,
                    expires_in_s: None,
                },
            );
            w.set_guest_share_text(line.text.as_str().into());
            w.set_guest_share_state(v.badge.as_str().into());
            w.set_guest_share_tone(match v.tone {
                1 => 1,
                2 => 2,
                _ => 0,
            });
        }
    });
}
