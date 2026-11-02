//! QObject exposing the dock model to QML.

use std::path::PathBuf;
use std::pin::Pin;
use std::time::Instant;

use cxx_qt::CxxQtType;
use cxx_qt_lib::QString;
use dock_model::{Dir, PANELS, Rect, WindowKind, Workspace};
use serde_json::json;

use crate::engine::Engine;
use crate::program_item::FRAME;

#[cxx_qt::bridge]
pub mod qobject {
    unsafe extern "C++" {
        include!("cxx-qt-lib/qstring.h");
        type QString = cxx_qt_lib::QString;
    }

    extern "RustQt" {
        #[qobject]
        #[qml_element]
        #[qproperty(QString, stacks)]
        #[qproperty(QString, handles)]
        #[qproperty(QString, panels)]
        #[qproperty(QString, floats)]
        #[qproperty(QString, workspace)]
        #[qproperty(QString, spectrum)]
        #[qproperty(QString, bars)]
        #[qproperty(bool, animating)]
        #[qproperty(i32, frame)]
        type Backend = super::BackendRust;

        #[qinvokable]
        fn relayout(self: Pin<&mut Backend>, w: f64, h: f64);
        #[qinvokable]
        fn activate(self: Pin<&mut Backend>, win: i32, stack: i32, tab: i32);
        #[qinvokable]
        fn closepanel(self: Pin<&mut Backend>, id: &QString);
        #[qinvokable]
        fn movenext(self: Pin<&mut Backend>, id: &QString);
        #[qinvokable]
        fn floatpanel(self: Pin<&mut Backend>, id: &QString);
        #[qinvokable]
        fn dock(self: Pin<&mut Backend>, win: i32);
        #[qinvokable]
        fn toggle(self: Pin<&mut Backend>, id: &QString);
        #[qinvokable]
        fn switchto(self: Pin<&mut Backend>, name: &QString);
        #[qinvokable]
        fn save(self: Pin<&mut Backend>);
        #[qinvokable]
        fn load(self: Pin<&mut Backend>);
        #[qinvokable]
        fn drag(self: Pin<&mut Backend>, handle: i32, delta: f64);
        #[qinvokable]
        fn tick(self: Pin<&mut Backend>);
        #[qinvokable]
        fn presented(self: Pin<&mut Backend>);
    }

    impl cxx_qt::Constructor<()> for Backend {}
}

/// Engine plus readback buffer. Qt does not share the engine's wgpu device,
/// so frames go GPU -> CPU -> Qt texture.
struct Gpu {
    engine: Engine,
    device: wgpu::Device,
    queue: wgpu::Queue,
    readback: wgpu::Buffer,
}

pub struct BackendRust {
    stacks: QString,
    handles: QString,
    panels: QString,
    floats: QString,
    workspace: QString,
    spectrum: QString,
    bars: QString,
    animating: bool,
    frame: i32,
    ws: Workspace,
    size: (f32, f32),
    flat_handles: Vec<dock_model::Handle>,
    gpu: Option<Gpu>,
    start: Instant,
    bands: Vec<f32>,
    path: String,
    fps: dock_model::FpsLog,
    measure: Option<dock_model::Measure>,
}

impl Default for BackendRust {
    fn default() -> Self {
        let mut ws = if std::env::var("RBS_PROTO_WORKSPACE").as_deref() == Ok("minimal") {
            Workspace::minimal()
        } else {
            Workspace::edit()
        };
        for id in std::env::var("RBS_PROTO_CLOSE")
            .unwrap_or_default()
            .split(',')
            .filter(|s| !s.is_empty())
        {
            ws.close(id);
        }
        BackendRust {
            stacks: QString::default(),
            handles: QString::default(),
            panels: QString::default(),
            floats: QString::default(),
            workspace: QString::default(),
            spectrum: QString::default(),
            bars: QString::default(),
            animating: false,
            frame: 0,
            ws,
            size: (1280.0, 764.0),
            flat_handles: Vec::new(),
            gpu: None,
            start: Instant::now(),
            bands: vec![0.0; dock_model::env::bands()],
            path: String::new(),
            fps: Default::default(),
            measure: dock_model::Measure::from_env("qt"),
        }
    }
}

impl cxx_qt::Initialize for qobject::Backend {
    fn initialize(self: Pin<&mut Self>) {
        self.sync();
    }
}

fn workspace_dir() -> PathBuf {
    std::env::var_os("RBS_PROTO_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("rbs-proto"))
}

fn stack_json(s: &dock_model::StackRect, win: usize) -> serde_json::Value {
    json!({
        "win": win, "stack": s.stack, "x": s.rect.x, "y": s.rect.y, "w": s.rect.w, "h": s.rect.h, "active": s.active,
        "tabs": s.tabs.iter().map(|id| json!({"id": id, "title": dock_model::title(id)})).collect::<Vec<_>>(),
    })
}

impl qobject::Backend {
    fn sync(mut self: Pin<&mut Self>) {
        let (w, h) = self.size;
        let flat = self.ws.flatten(
            0,
            Rect {
                x: 0.0,
                y: 0.0,
                w,
                h,
            },
        );
        let stacks = json!(
            flat.stacks
                .iter()
                .map(|s| stack_json(s, 0))
                .collect::<Vec<_>>()
        )
        .to_string();
        let handles = json!(flat
            .handles
            .iter()
            .enumerate()
            .map(|(i, h)| json!({"index": i, "x": h.rect.x, "y": h.rect.y, "w": h.rect.w, "h": h.rect.h, "horizontal": h.dir == Dir::Horizontal}))
            .collect::<Vec<_>>())
        .to_string();
        let panels = json!(
            PANELS
                .iter()
                .map(|(id, t)| json!({"id": id, "title": t, "open": self.ws.is_open(id)}))
                .collect::<Vec<_>>()
        )
        .to_string();
        let floats = json!(
            self.ws
                .windows
                .iter()
                .enumerate()
                .filter(|(_, w)| w.kind == WindowKind::Floating)
                .filter_map(|(i, _)| self
                    .ws
                    .flatten(
                        i,
                        Rect {
                            x: 0.0,
                            y: 0.0,
                            w: 640.0,
                            h: 368.0
                        }
                    )
                    .stacks
                    .first()
                    .map(|s| stack_json(s, i)))
                .collect::<Vec<_>>()
        )
        .to_string();
        let name = self.ws.name.clone();
        let visible = self.ws.visible_panels();
        let animate = visible
            .iter()
            .any(|p| p == "rbs.program" || p == "rbs.mixer");
        self.as_mut().rust_mut().flat_handles = flat.handles;
        self.as_mut().set_stacks(QString::from(&stacks));
        self.as_mut().set_handles(QString::from(&handles));
        self.as_mut().set_panels(QString::from(&panels));
        // Only touch the floats property when it changes, so floating windows are not recreated.
        if self.floats.to_string() != floats {
            self.as_mut().set_floats(QString::from(&floats));
        }
        self.as_mut().set_workspace(QString::from(&name));
        self.as_mut().set_animating(animate);
    }

    fn relayout(mut self: Pin<&mut Self>, w: f64, h: f64) {
        self.as_mut().rust_mut().size = (w as f32, h as f32);
        self.sync();
    }
    fn activate(mut self: Pin<&mut Self>, win: i32, stack: i32, tab: i32) {
        self.as_mut()
            .rust_mut()
            .ws
            .activate(win as usize, stack as usize, tab as usize);
        self.sync();
    }
    fn closepanel(mut self: Pin<&mut Self>, id: &QString) {
        self.as_mut().rust_mut().ws.close(&id.to_string());
        self.sync();
    }
    fn movenext(mut self: Pin<&mut Self>, id: &QString) {
        self.as_mut()
            .rust_mut()
            .ws
            .move_to_next_stack(&id.to_string());
        self.sync();
    }
    fn floatpanel(mut self: Pin<&mut Self>, id: &QString) {
        self.as_mut().rust_mut().ws.float(&id.to_string());
        self.sync();
    }
    fn dock(mut self: Pin<&mut Self>, win: i32) {
        self.as_mut().rust_mut().ws.dock(win as usize);
        self.sync();
    }
    fn toggle(mut self: Pin<&mut Self>, id: &QString) {
        let id = id.to_string();
        let ws = &mut self.as_mut().rust_mut().get_mut().ws;
        if ws.is_open(&id) {
            ws.close(&id)
        } else {
            ws.open(&id)
        }
        self.sync();
    }
    fn switchto(mut self: Pin<&mut Self>, name: &QString) {
        self.as_mut().rust_mut().ws = if name.to_string() == "Minimal" {
            Workspace::minimal()
        } else {
            Workspace::edit()
        };
        self.sync();
    }
    fn save(self: Pin<&mut Self>) {
        let dir = workspace_dir();
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("{}.toml", self.ws.name));
        match std::fs::write(&path, self.ws.to_toml()) {
            Ok(()) => eprintln!("saved {}", path.display()),
            Err(e) => eprintln!("save failed: {e}"),
        }
    }
    fn load(mut self: Pin<&mut Self>) {
        let path = workspace_dir().join(format!("{}.toml", self.ws.name));
        match std::fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|s| Workspace::from_toml(&s).map_err(|e| e.to_string()))
        {
            Ok(w) => self.as_mut().rust_mut().ws = w,
            Err(e) => eprintln!("load failed: {e}"),
        }
        self.sync();
    }
    fn drag(mut self: Pin<&mut Self>, handle: i32, delta: f64) {
        let r = self.as_mut().rust_mut().get_mut();
        if let Some(h) = r.flat_handles.get(handle as usize).cloned() {
            r.ws.drag_handle(0, &h, delta as f32);
        }
        self.sync();
    }

    /// Called on every swapped frame of the main window (measurement only).
    fn presented(mut self: Pin<&mut Self>) {
        if let Some(m) = self.as_mut().rust_mut().get_mut().measure.as_mut() {
            m.presented();
        }
    }

    /// One animation frame; called by a QML timer only while animated panels are visible.
    fn tick(mut self: Pin<&mut Self>) {
        let r = self.as_mut().rust_mut().get_mut();
        let t = r.start.elapsed().as_secs_f32();
        r.fps.tick();
        let visible = r.ws.visible_panels();
        let mut new_frame = false;
        if visible.iter().any(|p| p == "rbs.program") {
            let gpu = r.gpu.get_or_insert_with(Gpu::new);
            gpu.render_and_read(t);
            new_frame = true;
        }
        let mut spectrum = None;
        if visible.iter().any(|p| p == "rbs.mixer") {
            dock_model::spectrum(t, &mut r.bands);
            dock_model::spectrum_path(&r.bands, 1000.0, 1000.0, &mut r.path);
            let n = r.bands.len() as f32;
            let bw = 1000.0 / n;
            let mut bars = String::with_capacity(r.bands.len() * 40);
            for (i, v) in r.bands.iter().enumerate() {
                use std::fmt::Write;
                let _ = write!(
                    bars,
                    "M{:.1} 1000 v{:.1} h{:.1} v{:.1} Z ",
                    i as f32 * bw,
                    -v * 1000.0,
                    bw - 2.0,
                    v * 1000.0
                );
            }
            spectrum = Some((QString::from(&r.path), QString::from(&bars)));
        }
        if new_frame {
            let f = self.frame + 1;
            self.as_mut().set_frame(f);
        }
        if let Some((p, b)) = spectrum {
            self.as_mut().set_spectrum(p);
            self.as_mut().set_bars(b);
        }
    }
}

impl Gpu {
    fn new() -> Self {
        let instance = wgpu::Instance::default();
        let adapter = pollster::block_on(instance.request_adapter(&Default::default()))
            .expect("prototype: no wgpu adapter");
        let (device, queue) = pollster::block_on(adapter.request_device(&Default::default()))
            .expect("prototype: no wgpu device");
        let size = dock_model::env::engine_size();
        let engine = Engine::new(&device, &queue, size, wgpu::TextureUsages::COPY_SRC);
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: u64::from(size.0) * u64::from(size.1) * 4,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        Gpu {
            engine,
            device,
            queue,
            readback,
        }
    }

    fn render_and_read(&mut self, t: f32) {
        let (w, h) = self.engine.size;
        let tex = self.engine.render(t).clone();
        let mut enc = self.device.create_command_encoder(&Default::default());
        enc.copy_texture_to_buffer(
            tex.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &self.readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(w * 4),
                    rows_per_image: Some(h),
                },
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit([enc.finish()]);
        let slice = self.readback.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
        {
            let data = slice.get_mapped_range();
            if let Ok(mut f) = FRAME.lock() {
                f.0.clear();
                f.0.extend_from_slice(&data);
                f.1 = w as i32;
                f.2 = h as i32;
            }
        }
        self.readback.unmap();
    }
}
