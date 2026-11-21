//! The item transform requests of `every_request`, with a gesture (protocol 1.100).

use super::*;

pub(super) fn requests() -> Vec<Request> {
    let c = Request::Command;
    vec![
        c(Command::ItemFlip(FlipItem {
            gesture: 1 << 63 | 4,
            item: 7,
            axis: Axis::Vertical,
            canvas: OutputCanvas::Main,
        })),
        c(Command::ItemRotate(RotateItem {
            gesture: 0,
            item: 7,
            degrees: -90,
            canvas: OutputCanvas::Vertical,
        })),
        c(Command::ItemFit(FitItem {
            gesture: 0,
            item: 7,
            mode: FitKind::Fill,
            canvas: OutputCanvas::Vertical,
        })),
        c(Command::ItemAlign(AlignItem {
            gesture: 0,
            item: 7,
            axis: Some(AlignAxis::Horizontal),
            canvas: OutputCanvas::Main,
        })),
        c(Command::ItemResetTransform(ResetItemTransform {
            gesture: 0,
            item: 7,
            canvas: OutputCanvas::Vertical,
        })),
    ]
}
