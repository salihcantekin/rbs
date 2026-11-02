//! Qt (cxx-qt) prototype for the UI toolkit comparison (ADR-0022).
#![allow(missing_docs)]

pub mod backend;
pub mod program_item;

mod engine {
    use wgpu;
    // wgpu 27 takes plain bind group layout references.
    macro_rules! bgl_ref {
        ($b:expr) => {
            $b
        };
    }
    include!("../../shared/engine.rs");
}

use cxx_qt_lib::{QGuiApplication, QQmlApplicationEngine, QUrl};

fn main() {
    let mut app = QGuiApplication::new();
    let mut engine = QQmlApplicationEngine::new();
    if let Some(engine) = engine.as_mut() {
        engine.load(&QUrl::from("qrc:/qt/qml/com/rbs/proto/qml/main.qml"));
    }
    if let Some(app) = app.as_mut() {
        app.exec();
    }
}
