//! Source activity (ADR-0152): when a source runs, what it does when it returns,
//! and how long it lingers.
//!
//! A source type declares which values it supports and its default in
//! [`ActivityTraits`] (`NodeTypes::activity`). A source may store its own choice in
//! [`SourceActivity`]; [`ActivityTraits::effective`] merges the two. The binder
//! compares a source's [`Level`] with its effective [`Run`] on each sync; nothing
//! here runs per frame.

/// How far a source is from the screen, highest last (ADR-0152 decision 1).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Level {
    /// In no scene of the open collection.
    Unplaced,
    /// An item in some scene, hidden or off air.
    Placed,
    /// Drawn by a shown scene that is not on air: a canvas's studio-mode preview.
    Shown,
    /// Drawn by a program scene, a bound hold scene, or a running transition.
    OnAir,
}

/// The lowest [`Level`] at which a source is Active (ADR-0152 decision 3).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Run {
    /// Active while placed in any scene (capture, stills, audio; the wanted rule).
    Placed,
    /// Active while a shown scene draws it (media and browser by default).
    Shown,
    /// Active only while on air.
    OnAir,
}

impl Run {
    /// The level this value needs.
    pub const fn level(self) -> Level {
        match self {
            Run::Placed => Level::Placed,
            Run::Shown => Level::Shown,
            Run::OnAir => Level::OnAir,
        }
    }

    /// The saved spelling: `placed`, `shown` or `on_air`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Run::Placed => "placed",
            Run::Shown => "shown",
            Run::OnAir => "on_air",
        }
    }

    /// Parses [`Self::as_str`].
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "placed" => Some(Run::Placed),
            "shown" => Some(Run::Shown),
            "on_air" => Some(Run::OnAir),
            _ => None,
        }
    }
}

/// What a source does at its type's return edge (ADR-0152 decisions 3 and 6).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OnReturn {
    /// As if it had kept running while away.
    Continue,
    /// Where it stopped.
    Resume,
    /// From the beginning.
    Restart,
}

impl OnReturn {
    /// The saved spelling: `continue`, `resume` or `restart`.
    pub const fn as_str(self) -> &'static str {
        match self {
            OnReturn::Continue => "continue",
            OnReturn::Resume => "resume",
            OnReturn::Restart => "restart",
        }
    }

    /// Parses [`Self::as_str`].
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "continue" => Some(OnReturn::Continue),
            "resume" => Some(OnReturn::Resume),
            "restart" => Some(OnReturn::Restart),
            _ => None,
        }
    }
}

/// The longest linger a source may ask for, in seconds (ADR-0152 decision 3).
pub const MAX_LINGER_S: u16 = 600;

/// A source's saved activity choice (ADR-0152 decision 3). Every key is optional:
/// an absent key is the type's default. The binder never compares it with the
/// settings, so changing it never replaces the source's node.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SourceActivity {
    /// When the source runs; `None` is the type default.
    pub run: Option<Run>,
    /// What it does when it comes back; `None` is the type default.
    pub on_return: Option<OnReturn>,
    /// Seconds it stays Active after its level drops below `run`, at most
    /// [`MAX_LINGER_S`]; `None` is 0.
    pub linger_s: Option<u16>,
}

impl SourceActivity {
    /// A policy with the given keys.
    pub const fn new(run: Option<Run>, on_return: Option<OnReturn>, linger_s: Option<u16>) -> Self {
        Self {
            run,
            on_return,
            linger_s,
        }
    }

    /// True when every key is absent (the type default; left out of the file).
    pub const fn is_default(&self) -> bool {
        self.run.is_none() && self.on_return.is_none() && self.linger_s.is_none()
    }
}

/// What a source type supports (ADR-0152 decision 5), declared by the type through
/// `NodeTypes::activity`. The engine never tests a type id.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActivityTraits {
    /// The `run` values the type allows. Never empty; contains `default_run`.
    pub runs: &'static [Run],
    /// The `on_return` values the type allows; empty when it has none.
    pub on_returns: &'static [OnReturn],
    /// The default `run`.
    pub default_run: Run,
    /// The default `on_return`; `None` exactly when `on_returns` is empty.
    pub default_on_return: Option<OnReturn>,
    /// Whether a warm source (Active, drawn by no shown scene) stays linked to the
    /// program compositor of a canvas that may show it (`warm_sources`), as capture
    /// and stills are today.
    pub linked_while_warm: bool,
}

impl ActivityTraits {
    /// The traits of a type that says nothing: Active while placed, linked while
    /// warm, no `on_return`. Today's rule for capture, stills, audio and plugins.
    pub const PLACED: Self = Self {
        runs: &[Run::Placed],
        on_returns: &[],
        default_run: Run::Placed,
        default_on_return: None,
        linked_while_warm: true,
    };

    /// Traits with the given values. `default_run` must be in `runs` and
    /// `default_on_return` in `on_returns` (checked in debug builds by the tests of
    /// each type).
    pub const fn new(
        runs: &'static [Run],
        on_returns: &'static [OnReturn],
        default_run: Run,
        default_on_return: Option<OnReturn>,
        linked_while_warm: bool,
    ) -> Self {
        Self {
            runs,
            on_returns,
            default_run,
            default_on_return,
            linked_while_warm,
        }
    }

    /// Whether `activity` uses only values this type allows (and a linger in range).
    pub fn allows(&self, activity: &SourceActivity) -> bool {
        activity.run.is_none_or(|r| self.runs.contains(&r))
            && activity
                .on_return
                .is_none_or(|o| self.on_returns.contains(&o))
            && activity.linger_s.is_none_or(|s| s <= MAX_LINGER_S)
    }

    /// The policy in force for a source of this type that saved `saved`. A saved
    /// policy the type does not allow is ignored as a whole (the type default is
    /// used) and reported by the binder as unsupported; the saved value is kept.
    pub fn effective(&self, saved: Option<&SourceActivity>) -> Effective {
        match saved {
            Some(a) if self.allows(a) => Effective {
                run: a.run.unwrap_or(self.default_run),
                on_return: a.on_return.or(self.default_on_return),
                linger_s: a.linger_s.unwrap_or(0),
            },
            _ => Effective {
                run: self.default_run,
                on_return: self.default_on_return,
                linger_s: 0,
            },
        }
    }
}

impl Default for ActivityTraits {
    fn default() -> Self {
        Self::PLACED
    }
}

/// The policy in force for one source: its saved choice merged with its type's
/// defaults (ADR-0152 decision 3).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Effective {
    /// When it runs.
    pub run: Run,
    /// What it does when it comes back; `None` for types without the choice.
    pub on_return: Option<OnReturn>,
    /// Seconds it lingers.
    pub linger_s: u16,
}

impl Effective {
    /// Whether a source at `level` with this policy is Active (Off aside).
    pub fn active_at(&self, level: Level) -> bool {
        level >= self.run.level()
    }

    /// True when the source is not Active while merely placed (ADR-0107's temporal
    /// class, now by policy).
    pub fn is_temporal(&self) -> bool {
        self.run != Run::Placed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MEDIA: ActivityTraits = ActivityTraits::new(
        &[Run::Placed, Run::Shown, Run::OnAir],
        &[OnReturn::Continue, OnReturn::Resume, OnReturn::Restart],
        Run::Shown,
        Some(OnReturn::Restart),
        false,
    );

    #[test]
    fn levels_are_ordered_and_run_maps_to_a_level() {
        assert!(Level::Unplaced < Level::Placed);
        assert!(Level::Placed < Level::Shown);
        assert!(Level::Shown < Level::OnAir);
        let e = MEDIA.effective(None);
        assert!(!e.active_at(Level::Placed));
        assert!(e.active_at(Level::Shown));
        assert!(e.active_at(Level::OnAir));
        assert!(e.is_temporal());
        assert!(!ActivityTraits::PLACED.effective(None).is_temporal());
        assert!(
            !ActivityTraits::PLACED
                .effective(None)
                .active_at(Level::Unplaced)
        );
    }

    #[test]
    fn a_saved_policy_overrides_only_the_keys_it_sets() {
        let saved = SourceActivity::new(Some(Run::OnAir), None, Some(5));
        let e = MEDIA.effective(Some(&saved));
        assert_eq!(e.run, Run::OnAir);
        assert_eq!(e.on_return, Some(OnReturn::Restart));
        assert_eq!(e.linger_s, 5);
    }

    #[test]
    fn a_value_the_type_does_not_allow_falls_back_to_the_default() {
        let saved = SourceActivity::new(Some(Run::Shown), None, None);
        assert!(!ActivityTraits::PLACED.allows(&saved));
        assert_eq!(
            ActivityTraits::PLACED.effective(Some(&saved)).run,
            Run::Placed
        );
        let long = SourceActivity::new(None, None, Some(MAX_LINGER_S + 1));
        assert!(!MEDIA.allows(&long));
        assert_eq!(MEDIA.effective(Some(&long)).linger_s, 0);
    }

    #[test]
    fn spellings_round_trip() {
        for r in [Run::Placed, Run::Shown, Run::OnAir] {
            assert_eq!(Run::parse(r.as_str()), Some(r));
        }
        for o in [OnReturn::Continue, OnReturn::Resume, OnReturn::Restart] {
            assert_eq!(OnReturn::parse(o.as_str()), Some(o));
        }
        assert_eq!(Run::parse("always"), None);
    }
}
