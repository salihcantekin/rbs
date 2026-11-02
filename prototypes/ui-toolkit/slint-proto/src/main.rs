//! Slint prototype for the UI toolkit comparison (ADR-0020).
#![allow(missing_docs)]

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use dock_model::{Dir, PANELS, Rect, WindowKind, Workspace};
use slint::wgpu_30::wgpu;
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

slint::include_modules!();

mod engine {
    use super::wgpu;
    // wgpu 30 takes optional bind group layouts.
    macro_rules! bgl_ref {
        ($b:expr) => {
            Some($b)
        };
    }
    include!("../../shared/engine.rs");
}

struct State {
    ws: Workspace,
    size: (f32, f32),
    handles: Vec<dock_model::Handle>,
    floats: Vec<FloatWindow>,
    engine: engine::Engine,
    start: Instant,
    bands: Vec<f32>,
    path: String,
    fps: dock_model::FpsLog,
}

fn workspace_dir() -> PathBuf {
    std::env::var_os("RBS_PROTO_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("rbs-proto"))
}

fn to_stack(s: &dock_model::StackRect) -> Stack {
    let tabs: Vec<Tab> = s
        .tabs
        .iter()
        .map(|id| Tab {
            id: id.as_str().into(),
            title: dock_model::title(id).into(),
        })
        .collect();
    Stack {
        stack: s.stack as i32,
        x: s.rect.x,
        y: s.rect.y,
        w: s.rect.w,
        h: s.rect.h,
        tabs: ModelRc::new(VecModel::from(tabs)),
        active: s.active as i32,
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let t0 = Instant::now();
    // The engine owns the GPU device; Slint renders with the same device, so
    // program frames are shared as texture handles (zero copy).
    let instance = wgpu::Instance::default();
    let adapter = pollster::block_on(instance.request_adapter(&Default::default()))?;
    let (device, queue) = pollster::block_on(adapter.request_device(&Default::default()))?;
    eprintln!("adapter: {:?}", adapter.get_info().name);
    let engine = engine::Engine::new(&device, &queue, (1280, 720), wgpu::TextureUsages::empty());
    slint::BackendSelector::new()
        .require_wgpu_30(slint::wgpu_30::WGPUConfiguration::Manual {
            instance,
            adapter,
            device,
            queue,
        })
        .select()?;

    let ui = MainWindow::new()?;
    let mut ws = Workspace::edit();
    if std::env::var("RBS_PROTO_WORKSPACE").as_deref() == Ok("minimal") {
        ws = Workspace::minimal();
    }
    for id in std::env::var("RBS_PROTO_CLOSE")
        .unwrap_or_default()
        .split(',')
        .filter(|s| !s.is_empty())
    {
        ws.close(id);
    }
    let state = Rc::new(RefCell::new(State {
        ws,
        size: (1280.0, 764.0),
        handles: Vec::new(),
        floats: Vec::new(),
        engine,
        start: Instant::now(),
        bands: vec![0.0; 64],
        path: String::new(),
        fps: Default::default(),
    }));

    // Animation timer: runs only while a panel that needs frames is visible
    // (zero cost when unused, ADR-0013).
    let timer = Rc::new(slint::Timer::default());

    let sync: Rc<dyn Fn()> = {
        let ui = ui.as_weak();
        let state = state.clone();
        let timer = timer.clone();
        let sync_cell: Rc<RefCell<Option<Rc<dyn Fn()>>>> = Rc::new(RefCell::new(None));
        let sc = sync_cell.clone();
        let f: Rc<dyn Fn()> = Rc::new(move || {
            let Some(ui) = ui.upgrade() else { return };
            let mut st = state.borrow_mut();
            let (w, h) = st.size;
            let flat = st.ws.flatten(
                0,
                Rect {
                    x: 0.0,
                    y: 0.0,
                    w,
                    h,
                },
            );
            let stacks: Vec<Stack> = flat.stacks.iter().map(to_stack).collect();
            ui.set_stacks(ModelRc::new(VecModel::from(stacks)));
            let handles: Vec<Handle> = flat
                .handles
                .iter()
                .enumerate()
                .map(|(i, h)| Handle {
                    index: i as i32,
                    x: h.rect.x,
                    y: h.rect.y,
                    w: h.rect.w,
                    h: h.rect.h,
                    horizontal: h.dir == Dir::Horizontal,
                })
                .collect();
            st.handles = flat.handles;
            ui.set_handles(ModelRc::new(VecModel::from(handles)));
            let toggles: Vec<PanelToggle> = PANELS
                .iter()
                .map(|(id, t)| PanelToggle {
                    id: (*id).into(),
                    title: (*t).into(),
                    open: st.ws.is_open(id),
                })
                .collect();
            ui.set_panels(ModelRc::new(VecModel::from(toggles)));
            ui.set_workspace(st.ws.name.as_str().into());

            // Floating windows: one Slint window per floating workspace window.
            let floating: Vec<usize> = (0..st.ws.windows.len())
                .filter(|&i| st.ws.windows[i].kind == WindowKind::Floating)
                .collect();
            while st.floats.len() > floating.len() {
                if let Some(fw) = st.floats.pop() {
                    let _ = fw.hide();
                }
            }
            while st.floats.len() < floating.len() {
                let Ok(fw) = FloatWindow::new() else { break };
                let n = st.floats.len();
                let s2 = state.clone();
                let sc2 = sc.clone();
                fw.on_dock(move || {
                    let win = {
                        s2.borrow()
                            .ws
                            .windows
                            .iter()
                            .enumerate()
                            .filter(|(_, w)| w.kind == WindowKind::Floating)
                            .nth(n)
                            .map(|(i, _)| i)
                    };
                    if let Some(win) = win {
                        s2.borrow_mut().ws.dock(win);
                    }
                    if let Some(f) = sc2.borrow().clone() {
                        f();
                    }
                });
                let s3 = state.clone();
                let sc3 = sc.clone();
                fw.on_close_tab(move |id| {
                    s3.borrow_mut().ws.close(&id);
                    if let Some(f) = sc3.borrow().clone() {
                        f();
                    }
                });
                let _ = fw.show();
                st.floats.push(fw);
            }
            for (k, &win) in floating.iter().enumerate() {
                let flat = st.ws.flatten(
                    win,
                    Rect {
                        x: 0.0,
                        y: 0.0,
                        w: 640.0,
                        h: 368.0,
                    },
                );
                if let Some(s) = flat.stacks.first() {
                    let mut s = to_stack(s);
                    s.x = 0.0;
                    s.y = 0.0;
                    st.floats[k].set_data(s);
                }
            }

            let visible = st.ws.visible_panels();
            let animate = visible
                .iter()
                .any(|p| p == "rbs.program" || p == "rbs.mixer");
            drop(st);
            if animate && !timer.running() {
                let ui2 = ui.as_weak();
                let state2 = state.clone();
                timer.start(
                    slint::TimerMode::Repeated,
                    Duration::from_millis(16),
                    move || {
                        let Some(ui) = ui2.upgrade() else { return };
                        let mut st = state2.borrow_mut();
                        let t = st.start.elapsed().as_secs_f32();
                        st.fps.tick();
                        let visible = st.ws.visible_panels();
                        if visible.iter().any(|p| p == "rbs.program") {
                            let tex = st.engine.render(t).clone();
                            if let Ok(img) = slint::Image::try_from(tex) {
                                // Globals are per window in Slint, so every window gets the handle.
                                for fw in &st.floats {
                                    fw.global::<Shared>().set_program(img.clone());
                                }
                                ui.global::<Shared>().set_program(img);
                            }
                        }
                        if visible.iter().any(|p| p == "rbs.mixer") {
                            let State { bands, path, .. } = &mut *st;
                            dock_model::spectrum(t, bands);
                            dock_model::spectrum_path(bands, 1000.0, 1000.0, path);
                            let g = ui.global::<Shared>();
                            g.set_spectrum_path(SharedString::from(path.as_str()));
                            // Update the bands model in place to avoid re-allocating it each frame.
                            let m = g.get_bands();
                            if m.row_count() == bands.len() {
                                for (i, v) in bands.iter().enumerate() {
                                    m.set_row_data(i, *v);
                                }
                            } else {
                                g.set_bands(ModelRc::new(VecModel::from(bands.clone())));
                            }
                        }
                    },
                );
            } else if !animate && timer.running() {
                timer.stop();
            }
        });
        *sync_cell.borrow_mut() = Some(f.clone());
        f
    };

    macro_rules! on {
        ($setter:ident, |$st:ident $(, $arg:ident : $ty:ty)*| $body:block) => {{
            let state = state.clone();
            let sync = sync.clone();
            ui.$setter(move |$($arg: $ty),*| {
                { let $st = &mut *state.borrow_mut(); $body }
                sync();
            });
        }};
    }
    on!(on_relayout, |st, w: f32, h: f32| {
        st.size = (w, h);
    });
    on!(on_activate, |st, s: i32, t: i32| {
        st.ws.activate(0, s as usize, t as usize);
    });
    on!(on_close_tab, |st, id: SharedString| {
        st.ws.close(&id);
    });
    on!(on_move_next, |st, id: SharedString| {
        st.ws.move_to_next_stack(&id);
    });
    on!(on_float, |st, id: SharedString| {
        st.ws.float(&id);
    });
    on!(on_drag, |st, i: i32, d: f32| {
        if let Some(h) = st.handles.get(i as usize).cloned() {
            st.ws.drag_handle(0, &h, d);
        }
    });
    on!(on_switch, |st, name: SharedString| {
        st.ws = if name == "Minimal" {
            Workspace::minimal()
        } else {
            Workspace::edit()
        };
    });
    on!(on_toggle, |st, id: SharedString| {
        if st.ws.is_open(&id) {
            st.ws.close(&id)
        } else {
            st.ws.open(&id)
        }
    });
    on!(on_save, |st| {
        let dir = workspace_dir();
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("{}.toml", st.ws.name));
        match std::fs::write(&path, st.ws.to_toml()) {
            Ok(()) => eprintln!("saved {}", path.display()),
            Err(e) => eprintln!("save failed: {e}"),
        }
    });
    on!(on_load, |st| {
        let path = workspace_dir().join(format!("{}.toml", st.ws.name));
        match std::fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|s| Workspace::from_toml(&s).map_err(|e| e.to_string()))
        {
            Ok(w) => st.ws = w,
            Err(e) => eprintln!("load failed: {e}"),
        }
    });

    let first = Rc::new(std::cell::Cell::new(true));
    ui.window().set_rendering_notifier(move |s, _| {
        if matches!(s, slint::RenderingState::AfterRendering) && first.replace(false) {
            eprintln!("first frame after {} ms", t0.elapsed().as_millis());
        }
    })?;
    sync();
    ui.run()?;
    Ok(())
}
