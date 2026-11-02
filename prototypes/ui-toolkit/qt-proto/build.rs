use cxx_qt_build::{CxxQtBuilder, QmlModule};

fn main() {
    CxxQtBuilder::new_qml_module(QmlModule::new("com.rbs.proto").qml_file("qml/main.qml"))
        .qt_module("Quick")
        .qt_module("Network")
        .files(["src/backend.rs", "src/program_item.rs"])
        .build();
}
