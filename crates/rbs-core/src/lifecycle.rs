//! Installed / Standby / Active lifecycle of nodes, adapters and plugins (ADR-0013,
//! media graph spec "Lifecycle").

use std::fmt;

/// The lifecycle state of a node, adapter or plugin.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lifecycle {
    /// Known from metadata only; no resources.
    Installed,
    /// Configured but not scheduled; resources only when kept warm.
    Standby,
    /// Prepared and in the scheduled graph.
    Active,
}

/// A transition the lifecycle does not allow (states move one step at a time).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidTransition {
    /// Current state.
    pub from: Lifecycle,
    /// Requested state.
    pub to: Lifecycle,
}

impl fmt::Display for InvalidTransition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "cannot move from {:?} to {:?}", self.from, self.to)
    }
}

impl std::error::Error for InvalidTransition {}

impl Lifecycle {
    /// Checks a transition: Installed <-> Standby <-> Active, one step at a time.
    /// Staying in the same state is allowed.
    pub fn transition(self, to: Lifecycle) -> Result<Lifecycle, InvalidTransition> {
        use Lifecycle::*;
        match (self, to) {
            (a, b) if a == b => Ok(b),
            (Installed, Standby) | (Standby, Installed) | (Standby, Active) | (Active, Standby) => {
                Ok(to)
            }
            _ => Err(InvalidTransition { from: self, to }),
        }
    }

    /// True if nodes in this state are part of the scheduled graph.
    pub const fn is_scheduled(self) -> bool {
        matches!(self, Lifecycle::Active)
    }
}

#[cfg(test)]
mod tests {
    use super::Lifecycle::*;
    use super::*;

    #[test]
    fn transitions_are_one_step() {
        assert_eq!(Installed.transition(Standby), Ok(Standby));
        assert_eq!(Standby.transition(Active), Ok(Active));
        assert_eq!(Active.transition(Standby), Ok(Standby));
        assert_eq!(Standby.transition(Installed), Ok(Installed));
        assert_eq!(Active.transition(Active), Ok(Active));
        assert_eq!(
            Installed.transition(Active),
            Err(InvalidTransition {
                from: Installed,
                to: Active
            })
        );
        assert!(Active.transition(Installed).is_err());
        assert!(Active.is_scheduled());
        assert!(!Standby.is_scheduled());
    }
}
