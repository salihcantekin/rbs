//! Spike for the main window direction (round 2, option E): which docking pieces
//! Slint 1.18 provides on this platform. Not part of the app.
//!
//! ```sh
//! cargo run -p rbs-app --example dock_spike
//! ```
//!
//! Checks, each printed to stdout as it happens:
//! 1. `MenuBar` with shortcuts and checkable items (native menu bar on macOS and Windows).
//! 2. `ContextMenuArea` on a tab (right click).
//! 3. `DragArea` / `DropArea` inside one window: drag a tab onto the other stack; the drop
//!    position picks a compass target (left, right, top, bottom, centre = add as tab).
//! 4. The same drag onto the second window: Slint 1.18's winit backend has no native drag
//!    (`WindowAdapter::start_drag` returns false), so this is expected to fail.
//! 5. Tear-off with a plain `TouchArea`: press a tab's grip and release outside the window;
//!    the release position (window coordinates) shows whether pointer events keep coming
//!    outside the window while the button is held, which floating a panel needs.

#![allow(missing_docs, clippy::print_stdout, clippy::print_stderr, clippy::expect_used)] // A spike: generated types are undocumented.

use slint::{ComponentHandle, DataTransfer, SharedString};

slint::slint! {

    component Tab inherits Rectangle {
        in property <string> name;
        pure callback make-data(string) -> data-transfer;
        callback log(string);
        callback tear(string, length, length, bool);
        height: 30px;
        width: 120px;
        border-radius: 4px;
        background: drag.dragging ? #3b4a66 : #252931;
        ContextMenuArea {
            Menu {
                MenuItem { title: "Float"; activated => { root.log("context menu: float " + root.name); } }
                MenuItem { title: "Auto-hide"; activated => { root.log("context menu: auto-hide " + root.name); } }
                MenuItem { title: "Close"; activated => { root.log("context menu: close " + root.name); } }
            }
            drag := DragArea {
                allow-move: true;
                data: root.make-data(root.name);
                drag-finished(action) => { root.log("drag of " + root.name + " finished: " + (action == DragAction.move ? "move" : "none")); }
                Text { x: 26px; text: root.name; color: white; vertical-alignment: center; }
            }
            // Grip for the tear-off check (5): a plain TouchArea, not a DragArea.
            Rectangle {
                x: 4px; width: 18px; height: parent.height - 8px; y: 4px;
                background: grip.pressed ? rgb(110,168,255) : #4a5262;
                border-radius: 3px;
                grip := TouchArea {
                    moved => { root.tear(root.name, self.absolute-position.x + self.mouse-x, self.absolute-position.y + self.mouse-y, false); }
                    pointer-event(e) => {
                        if (e.kind == PointerEventKind.up) {
                            root.tear(root.name, self.absolute-position.x + self.mouse-x, self.absolute-position.y + self.mouse-y, true);
                        }
                    }
                }
            }
        }
    }

    component Stack inherits Rectangle {
        in property <string> title;
        in property <[string]> tabs;
        pure callback make-data(string) -> data-transfer;
        callback dropped-on(data-transfer, string, string); // payload, stack, target
        callback log(string);
        callback tear(string, length, length, bool);
        property <string> target: "";
        background: drop.has-drag ? #1d2b44 : #1b1e23;
        border-width: 1px;
        border-color: drop.has-drag ? #5b9cff : #2a2f37;
        function zone(px: length, py: length) -> string {
            if (px < self.width * 0.25) { return "left"; }
            if (px > self.width * 0.75) { return "right"; }
            if (py < self.height * 0.25) { return "top"; }
            if (py > self.height * 0.75) { return "bottom"; }
            return "centre";
        }
        drop := DropArea {
            can-drop(e) => { root.target = root.zone(e.position.x, e.position.y); return DragAction.move; }
            dropped(e) => { root.dropped-on(e.data, root.title, root.zone(e.position.x, e.position.y)); return e.proposed-action; }
        }
        VerticalLayout {
            padding: 8px; spacing: 6px; alignment: start;
            Text { text: root.title + (drop.has-drag ? "  (drop: " + root.target + ")" : ""); color: #a8afbb; }
            for t in root.tabs: Tab {
                name: t;
                make-data(n) => { return root.make-data(n); }
                log(s) => { root.log(s); }
                tear(n, x, y, up) => { root.tear(n, x, y, up); }
            }
        }
    }

    export component SpikeMain inherits Window {
        title: "RBS dock spike";
        width: 760px; height: 420px;
        background: #121417;
        in-out property <[string]> left: ["Scenes", "Sources", "Mixer"];
        in-out property <[string]> right: ["Inspector"];
        in-out property <bool> left-area: true;
        pure callback make-data(string) -> data-transfer;
        callback dropped-on(data-transfer, string, string);
        callback log(string);
        callback tear(string, length, length, bool);
        callback open-second();
        MenuBar {
            Menu {
                title: "View";
                MenuItem { title: "Left area"; checkable: true; checked <=> root.left-area; shortcut: @keys(Control + Alt + "1"); activated => { root.log("menu: left area " + (root.left-area ? "on" : "off")); } }
                MenuItem { title: "Studio mode"; checkable: true; shortcut: @keys(Control + Shift + S); activated => { root.log("menu: studio mode"); } }
                MenuSeparator { }
                MenuItem { title: "Customize toolbar…"; activated => { root.log("menu: customize toolbar"); } }
                Menu {
                    title: "Workspace";
                    MenuItem { title: "Edit"; activated => { root.log("menu: workspace Edit"); } }
                    MenuItem { title: "Live"; activated => { root.log("menu: workspace Live"); } }
                }
            }
            Menu {
                title: "Window";
                MenuItem { title: "Open second window"; activated => { root.open-second(); } }
            }
        }
        HorizontalLayout {
            padding: 10px; spacing: 10px;
            if root.left-area: Stack {
                title: "Left stack"; tabs: root.left;
                make-data(n) => { return root.make-data(n); }
                dropped-on(d, s, t) => { root.dropped-on(d, s, t); }
                log(s) => { root.log(s); }
                tear(n, x, y, up) => { root.tear(n, x, y, up); }
            }
            Stack {
                title: "Right stack"; tabs: root.right;
                make-data(n) => { return root.make-data(n); }
                dropped-on(d, s, t) => { root.dropped-on(d, s, t); }
                log(s) => { root.log(s); }
                tear(n, x, y, up) => { root.tear(n, x, y, up); }
            }
        }
    }

    export component SpikeSecond inherits Window {
        title: "RBS dock spike: second window";
        width: 320px; height: 240px;
        background: #121417;
        in-out property <[string]> tabs: [];
        pure callback make-data(string) -> data-transfer;
        callback dropped-on(data-transfer, string, string);
        callback log(string);
        callback tear(string, length, length, bool);
        Stack {
            title: "Second window";
            tabs: root.tabs;
            make-data(n) => { return root.make-data(n); }
            dropped-on(d, s, t) => { root.dropped-on(d, s, t); }
            log(s) => { root.log(s); }
            tear(n, x, y, up) => { root.tear(n, x, y, up); }
        }
    }
}

fn payload(name: SharedString) -> DataTransfer {
    let mut d = DataTransfer::default();
    d.set_plain_text(name);
    d
}

fn report_drop(d: DataTransfer, stack: SharedString, target: SharedString) {
    let name = d.plain_text().unwrap_or_default();
    println!("DROP {name} on {stack}, target {target}");
}

fn main() -> Result<(), slint::PlatformError> {
    let main = SpikeMain::new()?;
    let second = SpikeSecond::new()?;
    let size = main.window().size();
    println!("SPIKE start: main window {}x{} physical px", size.width, size.height);

    main.on_make_data(payload);
    main.on_dropped_on(report_drop);
    main.on_log(|s| println!("LOG {s}"));
    let weak = main.as_weak();
    main.on_tear(move |name, x, y, up| {
        let Some(w) = weak.upgrade() else { return };
        let scale = w.window().scale_factor();
        let size = w.window().size().to_logical(scale);
        let outside = x < 0.0 || y < 0.0 || x > size.width || y > size.height;
        if up {
            println!("TEAR released {name} at ({x:.0}, {y:.0}) outside={outside}");
        } else if outside {
            println!("TEAR moving {name} outside the window at ({x:.0}, {y:.0})");
        }
    });
    let second_weak = second.as_weak();
    main.on_open_second(move || {
        if let Some(s) = second_weak.upgrade() {
            let _ = s.show();
        }
    });

    second.on_make_data(payload);
    second.on_dropped_on(report_drop);
    second.on_log(|s| println!("LOG {s}"));
    second.on_tear(|name, x, y, up| {
        if up {
            println!("TEAR released {name} in second window at ({x:.0}, {y:.0})");
        }
    });
    second.show()?;
    main.run()
}
