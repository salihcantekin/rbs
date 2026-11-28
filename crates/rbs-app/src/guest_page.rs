//! The guest window (ADR-0139 phase 1d): Share (this RBS sends its program) and Add
//! guest (this RBS receives one). The pages and the calls are in `rbs_app::guest_view`;
//! this file owns the window, pushes the view into it and polls the pairing's state.

use std::time::Duration;

use rbs_app::guest_view::{Flow, GuestApi, Session, Unavailable};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use crate::ui::{GuestWindow, Theme, Tokens};
use crate::{App, Ctx, entry, look, window_place};

/// How often the open window reads the pairing's state, until the control API sends
/// `guest.changed` to the app (the event carries the same state).
const POLL: Duration = Duration::from_millis(500);

/// The open window and its session.
pub struct GuestPage {
    pub window: GuestWindow,
    session: Session,
    api: Box<dyn GuestApi>,
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
    let mut page = GuestPage {
        window: w,
        session: Session::new(flow),
        // Until the `guest.*` methods exist on the studio, every call says so.
        api: Box::new(Unavailable),
    };
    page.session.start(page.api.as_mut());
    page.push();
    let size = slint::LogicalSize::new(
        page.window.global::<Tokens>().get_size_guest_w(),
        page.window.get_wanted_height(),
    );
    let _ = window_place::show_centred(&page.window, &app.main, size, app.hidden);
    let _ = crate::exclude_window_from_capture(page.window.window(), app.bg.exclude_own_window);
    app.capture_applied.1 = usize::MAX;
    app.capture_retry = None;
    app.guest = Some(page);
    schedule(ctx);
}

/// Hides the window and ends a pairing that has not gone live.
fn close(app: &mut App) {
    if let Some(mut page) = app.guest.take() {
        use rbs_app::guest_view::State;
        // A running share or guest outlives the window; one still pairing does not.
        if page.session.status().state != State::Live {
            page.session.stop(page.api.as_mut());
        }
        let _ = page.window.hide();
    }
}

/// Reads the state again after [`POLL`], for as long as the window is open: a poll
/// with no pairing, or one that ended, asks nothing.
fn schedule(ctx: &Ctx) {
    let c = ctx.clone();
    entry::single_shot(POLL, move || {
        let again = c.clone();
        c.update(move |app| {
            let Some(page) = app.guest.as_mut() else {
                return;
            };
            page.session.poll(page.api.as_mut());
            page.push();
            schedule(&again);
        });
    });
}

impl GuestPage {
    /// Pushes what the session shows into the window.
    fn push(&self) {
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
                })
            });
        }};
    }
    on!(on_submit, |page, text: SharedString| {
        page.session.submit(page.api.as_mut(), text.as_str());
        page.window.set_pasted(SharedString::new());
        page.push();
    });
    on!(on_confirm, |page| {
        page.session.confirm(page.api.as_mut());
        page.push();
    });
    on!(on_mismatch, |page| {
        page.session.stop(page.api.as_mut());
        page.push();
    });
    on!(on_stop, |page| {
        page.session.stop(page.api.as_mut());
        page.push();
    });
    on!(on_again, |page| {
        page.session.again(page.api.as_mut());
        page.push();
    });
    {
        let c = ctx.clone();
        w.on_dismiss(move || {
            entry::guard(|| c.update(close));
        });
    }
}
