//! A member row selects that member, the group row and a picture the group (ADR-0194).

use rbs_app::studio_host::{SourceKind, SourceRow, SourceStatus, VerticalState};
use rbs_scene::model::{GroupId, ItemId, Layout, SourceId};

use crate::studio_ui::{StudioUi, properties_target::whole_group, tests::ui};

const GROUP: u32 = 7;

/// A colour source at `x` (100 px square on both canvases), in `group` if any.
fn row(id: u32, group: Option<u32>, x: f32) -> SourceRow {
    let at = Layout::new([x, 0.0], [100.0, 100.0]);
    SourceRow {
        id: SourceId::from_raw(id),
        name: format!("S{id}"),
        kind: Some(SourceKind::Color),
        status: SourceStatus::Running,
        meter: None,
        layout: Some(at),
        vertical_layout: Some(at),
        item: ItemId::from_raw(id + 100),
        companion_of: None,
        group: group.map(GroupId::from_raw),
        group_name: if group.is_some() {
            "Pair".into()
        } else {
            String::new()
        },
        picture: None,
        visible: true,
        active: true,
        locked: false,
        audio: Default::default(),
        heard: None,
        filters: Vec::new(),
        global: false,
        also_in: Vec::new(),
        in_globals: false,
        target: String::new(),
        glide: None,
        fill: String::new(),
        filled: true,
        cursor: None,
        text: None,
        browser: None,
        slideshow: None,
        browser_reason: None,
        cap_has: false,
        cap_l: 0,
        cap_t: 0,
        cap_r: 0,
        cap_b: 0,
        chrome_on: false,
        chrome_t: 0,
        chrome_r: 0,
        chrome_b: 0,
        chrome_l: 0,
        chrome_fill: 0,
        has_work_rect: false,
        show_for_ms: 0,
        scroll_paused: [false; 2],
        record_separately: Default::default(),
        device_peers: Vec::new(),
        latency: None,
        media: None,
        activity: None,
    }
}

/// A scene of sources 1, 2, 3 in group 7, then 4 alone, side by side.
fn studio(name: &str) -> (StudioUi, std::path::PathBuf) {
    let (mut ui, d) = ui(name);
    let g = Some(GROUP);
    ui.set_rows(vec![
        row(1, g, 0.0),
        row(2, g, 200.0),
        row(3, g, 400.0),
        row(4, None, 600.0),
    ]);
    (ui, d)
}

/// The selected ids and the group Properties would show.
fn shown(ui: &StudioUi) -> (Vec<i32>, Option<u32>) {
    let ids = ui.picked.ids(ui.selected);
    let group = whole_group(&ui.rows, &ids);
    (ids, group)
}

/// The middle of source `id`'s box on a picture the size of the canvas.
fn centre(id: u32) -> [f32; 2] {
    [(id - 1) as f32 * 200.0 + 50.0, 50.0]
}

fn canvas(ui: &StudioUi) -> [f32; 2] {
    [ui.canvas.width as f32, ui.canvas.height as f32]
}

#[test]
fn a_member_row_selects_the_member_and_the_group_row_the_group() {
    let (mut ui, d) = studio("member-row");
    // A plain click, Return or Space on a member row (`select-source`).
    ui.select_one(2);
    assert_eq!(shown(&ui), (vec![2], None), "the member's own Properties");
    ui.select_group(GROUP as i32);
    assert_eq!(shown(&ui), (vec![1, 2, 3], Some(GROUP)), "the group");
    ui.select_one(3);
    assert_eq!(shown(&ui), (vec![3], None), "from the group to one member");
    let _ = std::fs::remove_dir_all(d);
}

#[test]
fn control_click_on_a_member_of_a_selected_group_drops_it_and_the_group_target() {
    let (mut ui, d) = studio("member-toggle");
    ui.select_group(GROUP as i32);
    ui.select_with(2, true, false);
    assert_eq!(shown(&ui), (vec![1, 3], None), "the rest, no group target");
    ui.select_with(2, true, false);
    assert_eq!(
        shown(&ui),
        (vec![1, 2, 3], Some(GROUP)),
        "exactly the group"
    );
    let _ = std::fs::remove_dir_all(d);
}

#[test]
fn a_picture_press_selects_the_group_unless_the_member_is_selected_alone() {
    let (mut ui, d) = studio("member-picture");
    let size = canvas(&ui);
    ui.select_one(4);
    ui.preview_press(centre(2), size, false, false, false);
    ui.preview_release();
    assert_eq!(
        shown(&ui),
        (vec![1, 2, 3], Some(GROUP)),
        "an unselected member"
    );
    ui.select_one(2);
    ui.preview_press(centre(2), size, false, false, false);
    ui.preview_release();
    assert_eq!(shown(&ui), (vec![2], None), "the member chosen in the list");
    // A right-click on it keeps it too; on another member it selects the group.
    assert_eq!(ui.preview_source_at(centre(2), size), Some(2));
    assert_eq!(shown(&ui), (vec![2], None));
    assert_eq!(ui.preview_source_at(centre(3), size), Some(3));
    assert_eq!(shown(&ui), (vec![1, 2, 3], Some(GROUP)));
    let _ = std::fs::remove_dir_all(d);
}

#[test]
fn the_vertical_picture_keeps_a_member_selected_alone_and_else_selects_the_group() {
    let (mut ui, d) = studio("member-vertical");
    ui.vertical = Some(VerticalState {
        canvas: rbs_scene::model::CanvasId::from_raw(2),
        size: rbs_core::media::video::Size::new(1080, 1920),
        follow: true,
    });
    let size = [1080.0, 1920.0];
    ui.select_one(2);
    assert_eq!(ui.vertical_source_at(centre(2), size), Some(2));
    assert_eq!(shown(&ui), (vec![2], None), "selected alone: kept");
    ui.select_one(4);
    assert_eq!(ui.vertical_source_at(centre(2), size), Some(2));
    assert_eq!(shown(&ui), (vec![1, 2, 3], Some(GROUP)), "else the group");
    let _ = std::fs::remove_dir_all(d);
}

#[test]
fn the_list_filter_hides_by_category_and_keeps_groups_and_the_selection() {
    use crate::studio_ui::list_filter::filtered_out;
    let mut hidden = row(2, None, 0.0);
    hidden.visible = false;
    let rows = vec![row(1, Some(GROUP), 0.0), hidden, row(3, Some(GROUP), 0.0)];
    let mut f = rbs_user_data::PanelFilters::default();
    assert_eq!(filtered_out(&rows, &f, &[]), [false, false, false]);

    f.show_hidden_sources = false;
    assert_eq!(filtered_out(&rows, &f, &[]), [false, true, false]);
    assert_eq!(filtered_out(&rows, &f, &[2]), [false, false, false]);

    f.hidden_source_categories = vec!["picture".to_owned()];
    assert_eq!(filtered_out(&rows, &f, &[]), [true, true, true]);
    assert_eq!(filtered_out(&rows, &f, &[3]), [true, true, false]);
}
