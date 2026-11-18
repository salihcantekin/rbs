//! Properties' "Runs" and "When it comes back" (ADR-0152): the option lists'
//! positions and the activity values they stand for. The lists' words are in the
//! `.slint` files, in the same order.

use rbs_scene::activity::{OnReturn, Run};

use crate::studio_host::{ActivityChoice, SourceKind};

/// A web page: "While in a scene", "While shown", "Only on air".
const BROWSER_RUNS: [Run; 3] = [Run::Placed, Run::Shown, Run::OnAir];

/// A media file: "While shown", "Only on air". `placed` is left out until a film
/// that no shown scene draws is really kept open (the engine prepares only scheduled
/// nodes); a film saved with it shows no choice.
const MEDIA_RUNS: [Run; 2] = [Run::Shown, Run::OnAir];

/// A media file: "Start over", "Resume where it stopped".
const MEDIA_RETURNS: [OnReturn; 2] = [OnReturn::Restart, OnReturn::Resume];

/// A web page: "Continue", "Reload".
const BROWSER_RETURNS: [OnReturn; 2] = [OnReturn::Continue, OnReturn::Restart];

/// The "Runs" list of a kind; empty for a kind without one.
fn runs(kind: Option<SourceKind>) -> &'static [Run] {
    match kind {
        Some(SourceKind::Media) => &MEDIA_RUNS,
        Some(SourceKind::Browser) => &BROWSER_RUNS,
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

#[cfg(test)]
mod tests {
    use super::*;

    const MEDIA: Option<SourceKind> = Some(SourceKind::Media);
    const BROWSER: Option<SourceKind> = Some(SourceKind::Browser);

    fn choice(run: Run, on_return: OnReturn) -> Option<ActivityChoice> {
        Some(ActivityChoice {
            run,
            on_return: Some(on_return),
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
        assert_eq!(with_run(MEDIA, film, 2), None);
        assert_eq!(with_run(MEDIA, film, -1), None);
        assert_eq!(with_return(BROWSER, None, 0), None);
    }
}
