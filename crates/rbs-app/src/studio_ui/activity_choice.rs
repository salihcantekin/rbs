//! Properties' "Runs", "When it comes back" and "Keep running for" (ADR-0152): the
//! option lists' positions and the activity values they stand for, and the calls
//! that send a pick. The lists' words are in the `.slint` files, in the same order.

use rbs_scene::activity::{OnReturn, Run};
use rbs_scene::model::SourceId;

use super::StudioUi;
use crate::studio_host::{ActivityChoice, Command, SourceKind};

/// A web page or a display, window or camera: "While in a scene", "While shown",
/// "Only on air".
const PLACED_RUNS: [Run; 3] = [Run::Placed, Run::Shown, Run::OnAir];

/// A media file: "While shown", "Only on air". `placed` is left out until a film
/// that no shown scene draws is really kept open (the engine prepares only scheduled
/// nodes); a film saved with it shows no choice.
const MEDIA_RUNS: [Run; 2] = [Run::Shown, Run::OnAir];

/// A media file: "Start over", "Resume where it stopped", "Continue as if it kept
/// playing".
const MEDIA_RETURNS: [OnReturn; 3] = [OnReturn::Restart, OnReturn::Resume, OnReturn::Continue];

/// A web page: "Continue", "Reload".
const BROWSER_RETURNS: [OnReturn; 2] = [OnReturn::Continue, OnReturn::Restart];

/// "Keep running for", in seconds: "Off", "5 s", "10 s", "30 s", "1 min", "5 min".
const LINGERS: [u16; 6] = [0, 5, 10, 30, 60, 300];

/// The "Runs" list of a kind; empty for a kind without one.
fn runs(kind: Option<SourceKind>) -> &'static [Run] {
    match kind {
        Some(SourceKind::Media) => &MEDIA_RUNS,
        Some(
            SourceKind::Browser | SourceKind::Screen | SourceKind::Window | SourceKind::Camera,
        ) => &PLACED_RUNS,
        _ => &[],
    }
}

/// The "When it comes back" list of a kind; empty for a kind without one.
fn returns(kind: Option<SourceKind>) -> &'static [OnReturn] {
    match kind {
        Some(SourceKind::Media) => &MEDIA_RETURNS,
        Some(SourceKind::Browser) => &BROWSER_RETURNS,
        _ => &[],
    }
}

/// The "Runs" position of a policy for a source of `kind`; -1 for none.
pub fn run_index(kind: Option<SourceKind>, choice: Option<ActivityChoice>) -> i32 {
    choice
        .and_then(|c| runs(kind).iter().position(|r| *r == c.run))
        .map_or(-1, |i| i as i32)
}

/// The "When it comes back" position of a policy for a source of `kind`; -1 for
/// none.
pub fn return_index(kind: Option<SourceKind>, choice: Option<ActivityChoice>) -> i32 {
    choice
        .and_then(|c| c.on_return)
        .and_then(|o| returns(kind).iter().position(|r| *r == o))
        .map_or(-1, |i| i as i32)
}

/// The "Keep running for" position of a policy; -1 for none or a time the list
/// leaves out (set through the API).
pub fn linger_index(choice: Option<ActivityChoice>) -> i32 {
    choice
        .and_then(|c| LINGERS.iter().position(|s| *s == c.linger_s))
        .map_or(-1, |i| i as i32)
}

/// The last warm-up of a capture source in seconds with one decimal ("0.6"); empty
/// when none is known.
pub fn warm_up_text(choice: Option<ActivityChoice>) -> String {
    choice
        .and_then(|c| c.warm_up_ms)
        .map_or_else(String::new, |ms| format!("{:.1}", f64::from(ms) / 1000.0))
}

/// The policy after "Runs" picks position `index` for a source of `kind`; `None`
/// for a position out of range or a source without a choice.
pub fn with_run(
    kind: Option<SourceKind>,
    choice: Option<ActivityChoice>,
    index: i32,
) -> Option<ActivityChoice> {
    let run = *runs(kind).get(usize::try_from(index).ok()?)?;
    Some(ActivityChoice { run, ..choice? })
}

/// The policy after "When it comes back" picks position `index` for a source of
/// `kind`; `None` for a position out of range or a source without a choice.
pub fn with_return(
    kind: Option<SourceKind>,
    choice: Option<ActivityChoice>,
    index: i32,
) -> Option<ActivityChoice> {
    let on_return = *returns(kind).get(usize::try_from(index).ok()?)?;
    Some(ActivityChoice {
        on_return: Some(on_return),
        ..choice?
    })
}

/// The policy after "Keep running for" picks position `index`; `None` for a
/// position out of range or a source without a choice.
pub fn with_linger(choice: Option<ActivityChoice>, index: i32) -> Option<ActivityChoice> {
    let linger_s = *LINGERS.get(usize::try_from(index).ok()?)?;
    Some(ActivityChoice {
        linger_s,
        ..choice?
    })
}

impl StudioUi {
    /// Properties' "Runs" picked position `index` for source `id` (ADR-0152).
    pub fn set_source_run(&mut self, id: i32, index: i32) {
        self.pick_activity(id, |kind, choice| with_run(kind, choice, index));
    }

    /// Properties' "When it comes back" picked position `index` for source `id`
    /// (ADR-0152).
    pub fn set_source_on_return(&mut self, id: i32, index: i32) {
        self.pick_activity(id, |kind, choice| with_return(kind, choice, index));
    }

    /// Properties' "Keep running for" picked position `index` for source `id`
    /// (ADR-0152 decision 3).
    pub fn set_source_linger(&mut self, id: i32, index: i32) {
        self.pick_activity(id, |_, choice| with_linger(choice, index));
    }

    /// The scroll's "When it comes back" picked "Start over" (0) or "Continue" (1)
    /// for source `id` (ADR-0152 phase 5); one undo step. Nothing without a scroll.
    pub fn set_source_scroll_return(&mut self, id: i32, index: i32) {
        let Some(scroll) = self.rows_scroll(id) else {
            return;
        };
        let on_return = if index == 1 {
            OnReturn::Continue
        } else {
            OnReturn::Restart
        };
        if scroll.on_return == on_return {
            return;
        }
        self.deleted.clear();
        self.send(Command::SetSourceScroll {
            gesture: 0,
            id: SourceId::from_raw(id as u32),
            scroll: Some(scroll.with_on_return(on_return)),
            canvas: self.props_target(id),
        });
    }

    fn pick_activity(
        &mut self,
        id: i32,
        pick: impl FnOnce(Option<SourceKind>, Option<ActivityChoice>) -> Option<ActivityChoice>,
    ) {
        let Some(row) = self.rows.iter().find(|r| r.id.get() as i32 == id) else {
            return;
        };
        let Some(choice) = pick(row.kind, row.activity) else {
            return;
        };
        let id: SourceId = row.id;
        self.deleted.clear();
        self.send(Command::SetSourceActivity {
            id,
            run: choice.run,
            on_return: choice.on_return,
            linger_s: choice.linger_s,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MEDIA: Option<SourceKind> = Some(SourceKind::Media);
    const BROWSER: Option<SourceKind> = Some(SourceKind::Browser);

    fn choice(run: Run, on_return: OnReturn) -> Option<ActivityChoice> {
        Some(ActivityChoice {
            run,
            on_return: Some(on_return),
            linger_s: 0,
            warm_up_ms: None,
        })
    }

    #[test]
    fn positions_follow_the_lists_of_each_kind() {
        let film = choice(Run::Shown, OnReturn::Restart);
        assert_eq!(run_index(MEDIA, film), 0);
        assert_eq!(return_index(MEDIA, film), 0);
        // A film saved with `placed` (through the API) shows no choice.
        assert_eq!(run_index(MEDIA, choice(Run::Placed, OnReturn::Restart)), -1);
        let page = choice(Run::Placed, OnReturn::Restart);
        assert_eq!(run_index(BROWSER, page), 0);
        assert_eq!(return_index(BROWSER, page), 1);
        assert_eq!(run_index(BROWSER, None), -1);
        assert_eq!(return_index(Some(SourceKind::Text), page), -1);
    }

    #[test]
    fn a_pick_changes_only_its_own_value() {
        let film = choice(Run::Shown, OnReturn::Restart);
        assert_eq!(
            with_run(MEDIA, film, 1),
            choice(Run::OnAir, OnReturn::Restart)
        );
        assert_eq!(
            with_run(BROWSER, film, 0),
            choice(Run::Placed, OnReturn::Restart)
        );
        assert_eq!(
            with_return(MEDIA, film, 1),
            choice(Run::Shown, OnReturn::Resume)
        );
        assert_eq!(
            with_return(MEDIA, film, 2),
            choice(Run::Shown, OnReturn::Continue)
        );
        assert_eq!(with_run(MEDIA, film, 2), None);
        assert_eq!(with_run(MEDIA, film, -1), None);
        assert_eq!(with_return(BROWSER, None, 0), None);
    }

    #[test]
    fn a_capture_source_has_runs_and_no_return() {
        let display = Some(SourceKind::Screen);
        let shown = Some(ActivityChoice {
            run: Run::Shown,
            on_return: None,
            linger_s: 0,
            warm_up_ms: Some(640),
        });
        assert_eq!(run_index(display, shown), 1);
        assert_eq!(return_index(display, shown), -1);
        assert_eq!(
            with_run(Some(SourceKind::Camera), shown, 0).map(|c| c.run),
            Some(Run::Placed)
        );
        assert_eq!(with_return(display, shown, 0), None);
        assert_eq!(warm_up_text(shown), "0.6");
        assert_eq!(warm_up_text(choice(Run::Shown, OnReturn::Restart)), "");
    }

    #[test]
    fn keep_running_for_picks_a_listed_time() {
        let film = choice(Run::Shown, OnReturn::Restart);
        assert_eq!(linger_index(film), 0);
        let held = with_linger(film, 3);
        assert_eq!(held.map(|c| c.linger_s), Some(30));
        assert_eq!(linger_index(held), 3);
        assert_eq!(with_linger(film, 6), None);
        assert_eq!(with_linger(None, 1), None);
        // A time set through the API that the list leaves out shows none.
        let odd = film.map(|c| ActivityChoice { linger_s: 20, ..c });
        assert_eq!(linger_index(odd), -1);
    }
}
