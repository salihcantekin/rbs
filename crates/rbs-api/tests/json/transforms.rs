//! The item transform requests of `every_request`, with a gesture, and the item edits
//! of protocol 1.100.

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
        c(Command::ItemMove(MoveItem {
            item: 7,
            direction: Some(LayerDirection::Up),
            to_item: None,
        })),
        c(Command::ItemMove(MoveItem {
            item: 7,
            direction: None,
            to_item: Some(9),
        })),
        c(Command::SceneMove(MoveScene {
            scene: 2,
            direction: Some(LayerDirection::Down),
            to_scene: None,
        })),
        c(Command::SceneMove(MoveScene {
            scene: 2,
            direction: None,
            to_scene: Some(3),
        })),
        c(Command::ItemFitTextBox(FitTextBox {
            item: 7,
            canvas: OutputCanvas::Main,
        })),
        c(Command::ItemSetFilled(SetItemFilled {
            item: 7,
            on: false,
            canvas: OutputCanvas::Vertical,
        })),
        c(Command::ItemSetShownRegion(SetItemShownRegion {
            item: 7,
            region: ShownRegion::CanvasCentred,
            canvas: OutputCanvas::Main,
        })),
        c(Command::FilterAddVoiceChain(SourceRef { source: 3 })),
        c(Command::CollectionSetStingerFile(SetStingerFile {
            path: "/media/sting.mov".to_owned(),
        })),
        c(Command::ItemSetRotation(SetItemRotation {
            item: 7,
            degrees: 22.5,
            canvas: OutputCanvas::Vertical,
            gesture: 3,
        })),
        c(Command::SourceAdd(AddSource {
            kind: SourceKind::Color,
            adapter: String::new(),
            target: String::new(),
            name: None,
            scene: Some(2),
            global: false,
            with_audio: false,
            text: None,
            stamp: Some(Stamp::RoundedRectangle),
            path: None,
            url: None,
        })),
        c(Command::SourceAdd(AddSource {
            kind: SourceKind::Media,
            adapter: String::new(),
            target: String::new(),
            name: Some("Clip".into()),
            scene: None,
            global: false,
            with_audio: false,
            text: None,
            stamp: None,
            path: Some("/media/clip.mov".into()),
            url: None,
        })),
        c(Command::SourceAdd(AddSource {
            stamp: None,
            path: None,
            url: None,
            kind: SourceKind::Screen,
            adapter: "wgc".into(),
            target: "display-1".into(),
            name: None,
            scene: None,
            global: true,
            with_audio: true,
            text: None,
        })),
        c(Command::SourceAdd(AddSource {
            stamp: None,
            path: None,
            url: None,
            kind: SourceKind::Text,
            adapter: String::new(),
            target: String::new(),
            name: Some("Title".into()),
            scene: Some(2),
            global: false,
            with_audio: false,
            text: Some("Hello".into()),
        })),
    ]
}
