//! Align and Distribute for the selected sources (protocol 1.106, ADR-0182): which items
//! they would move and the control API calls they send. Runs on a menu command, never
//! per frame.

use rbs_api::Command as C;
use rbs_api::request as rq;

use crate::studio_ui::StudioUi;

/// What an Arrange entry does to the selected sources.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArrangeOp {
    /// Group (`true`) or ungroup (`false`) the selected sources.
    Group(bool),
    /// Line the sources up on an edge or centre line of the selection.
    Align(rq::AlignEdge),
    /// Spread three or more sources so the gaps between them are equal.
    Distribute(rq::Axis),
}

impl ArrangeOp {
    /// How many sources the command needs.
    pub const fn needs(self) -> usize {
        match self {
            Self::Group(_) | Self::Align(_) => 2,
            Self::Distribute(_) => 3,
        }
    }
}

impl StudioUi {
    /// The items of the selected sources that have a picture, or why the command cannot
    /// run: Align needs two, Distribute three.
    pub fn arrange_items(&self, op: ArrangeOp) -> Result<Vec<u32>, &'static str> {
        let need = op.needs();
        let items: Vec<u32> = self
            .picked
            .ids(self.selected)
            .iter()
            .filter_map(|id| self.rows.iter().find(|r| r.id.get() as i32 == *id))
            .filter(|r| r.layout.is_some())
            .map(|r| r.item.get())
            .collect();
        if items.len() >= need {
            Ok(items)
        } else if need == 2 {
            Err("Select two or more sources with a picture first (Ctrl or Cmd+click).")
        } else {
            Err("Select three or more sources with a picture first (Ctrl or Cmd+click).")
        }
    }

    /// Align or distribute the selected sources as one undo step.
    pub fn arrange_command(&mut self, op: ArrangeOp) {
        if let ArrangeOp::Group(group) = op {
            return self.group_command(group);
        }
        let Ok(items) = self.arrange_items(op) else {
            return;
        };
        let canvas = self.props_target(self.selected);
        self.call(match op {
            ArrangeOp::Group(_) => return,
            ArrangeOp::Align(edge) => C::ItemAlignItems(rq::AlignItems {
                items,
                edge,
                to: rq::AlignTo::Selection,
                canvas,
                gesture: 0,
            }),
            ArrangeOp::Distribute(axis) => C::ItemDistribute(rq::DistributeItems {
                items,
                axis,
                canvas,
                gesture: 0,
            }),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn align_needs_two_sources_and_distribute_three() {
        assert_eq!(ArrangeOp::Align(rq::AlignEdge::Left).needs(), 2);
        assert_eq!(ArrangeOp::Distribute(rq::Axis::Vertical).needs(), 3);
    }
}
