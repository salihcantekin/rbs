//! iced prototype for the UI toolkit comparison (ADR-0020).
#![allow(missing_docs)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;

use dock_model::{Dir, Handle, Node, PANELS, Rect, WindowKind, Workspace};
use iced::wgpu;
use iced::widget::canvas::{self, Path, Stroke};
use iced::widget::shader::{self, Viewport};
use iced::widget::{
    button, canvas as canvas_w, column, container, mouse_area, row, shader as shader_w, text,
};
use iced::{
    Color, Element, Event, Length, Point, Rectangle, Size, Subscription, Task, Theme, event, mouse,
    window,
};

mod engine {
    use super::wgpu;
    // wgpu 27 takes plain bind group layout references.
    macro_rules! bgl_ref {
        ($b:expr) => {
            $b
        };
    }
    include!("../../shared/engine.rs");
}

const BAR: f32 = 36.0;
/// Mixed left-to-right and right-to-left text for the bidi check.
const BIDI: &str = "Go live: ابدأ البث الآن (3 outputs) · שידור חי 12:30";

#[derive(Debug, Clone)]
enum Msg {
    Tick(Instant),
    Activate(usize, usize, usize),
    Close(String),
    MoveNext(String),
    Float(String),
    Dock(window::Id),
    Toggle(String),
    Switch(&'static str),
    Save,
    Load,
    DragStart(usize),
    DragMove(Point),
    DragEnd,
    Resized(window::Id, Size),
    Opened(#[allow(dead_code)] window::Id),
    Closed(window::Id),
}

struct App {
    ws: Workspace,
    main: window::Id,
    floats: BTreeMap<window::Id, ()>,
    size: Size,
    start: Instant,
    t: f32,
    bands: Vec<f32>,
    spectrum: canvas::Cache,
    drag: Option<(usize, Option<Point>)>,
    handles: Vec<Handle>,
    first_frame: Option<Instant>,
    fps: dock_model::FpsLog,
}

fn workspace_dir() -> PathBuf {
    std::env::var_os("RBS_PROTO_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("rbs-proto"))
}

impl App {
    fn boot() -> (Self, Task<Msg>) {
        let (main, open) = window::open(window::Settings {
            size: Size::new(1280.0, 800.0),
            ..Default::default()
        });
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
        let app = App {
            ws,
            main,
            floats: BTreeMap::new(),
            size: Size::new(1280.0, 800.0),
            start: Instant::now(),
            t: 0.0,
            bands: vec![0.0; 64],
            spectrum: canvas::Cache::new(),
            drag: None,
            handles: Vec::new(),
            first_frame: Some(Instant::now()),
            fps: Default::default(),
        };
        (app, open.map(Msg::Opened))
    }

    fn relayout(&mut self) {
        let flat = self.ws.flatten(
            0,
            Rect {
                x: 0.0,
                y: 0.0,
                w: self.size.width,
                h: self.size.height - BAR,
            },
        );
        self.handles = flat.handles;
    }

    /// Open or close OS windows so they match the floating windows of the workspace.
    fn sync_windows(&mut self) -> Task<Msg> {
        let wanted = self
            .ws
            .windows
            .iter()
            .filter(|w| w.kind == WindowKind::Floating)
            .count();
        let mut tasks = Vec::new();
        while self.floats.len() > wanted {
            if let Some((id, _)) = self.floats.pop_last() {
                tasks.push(window::close(id));
            }
        }
        while self.floats.len() < wanted {
            let (id, t) = window::open(window::Settings {
                size: Size::new(640.0, 400.0),
                ..Default::default()
            });
            self.floats.insert(id, ());
            tasks.push(t.map(Msg::Opened));
        }
        self.relayout();
        Task::batch(tasks)
    }

    fn float_index(&self, id: window::Id) -> Option<usize> {
        let k = self.floats.keys().position(|w| *w == id)?;
        self.ws
            .windows
            .iter()
            .enumerate()
            .filter(|(_, w)| w.kind == WindowKind::Floating)
            .nth(k)
            .map(|(i, _)| i)
    }

    fn update(&mut self, msg: Msg) -> Task<Msg> {
        match msg {
            Msg::Tick(now) => {
                self.t = (now - self.start).as_secs_f32();
                self.fps.tick();
                if self.ws.visible_panels().iter().any(|p| p == "rbs.mixer") {
                    dock_model::spectrum(self.t, &mut self.bands);
                    self.spectrum.clear();
                }
                if let Some(t0) = self.first_frame.take() {
                    eprintln!("first tick after {} ms", t0.elapsed().as_millis());
                }
            }
            Msg::Activate(win, s, t) => self.ws.activate(win, s, t),
            Msg::Close(id) => self.ws.close(&id),
            Msg::MoveNext(id) => self.ws.move_to_next_stack(&id),
            Msg::Float(id) => {
                self.ws.float(&id);
            }
            Msg::Dock(w) => {
                if let Some(i) = self.float_index(w) {
                    self.ws.dock(i);
                }
            }
            Msg::Toggle(id) => {
                if self.ws.is_open(&id) {
                    self.ws.close(&id)
                } else {
                    self.ws.open(&id)
                }
            }
            Msg::Switch(name) => {
                self.ws = if name == "Minimal" {
                    Workspace::minimal()
                } else {
                    Workspace::edit()
                }
            }
            Msg::Save => {
                let dir = workspace_dir();
                let _ = std::fs::create_dir_all(&dir);
                let path = dir.join(format!("{}.toml", self.ws.name));
                match std::fs::write(&path, self.ws.to_toml()) {
                    Ok(()) => eprintln!("saved {}", path.display()),
                    Err(e) => eprintln!("save failed: {e}"),
                }
            }
            Msg::Load => {
                let path = workspace_dir().join(format!("{}.toml", self.ws.name));
                match std::fs::read_to_string(&path)
                    .map_err(|e| e.to_string())
                    .and_then(|s| Workspace::from_toml(&s).map_err(|e| e.to_string()))
                {
                    Ok(w) => self.ws = w,
                    Err(e) => eprintln!("load failed: {e}"),
                }
            }
            Msg::DragStart(i) => self.drag = Some((i, None)),
            Msg::DragMove(p) => {
                if let Some((i, last)) = self.drag {
                    if let (Some(last), Some(h)) = (last, self.handles.get(i).cloned()) {
                        let d = if h.dir == Dir::Horizontal {
                            p.x - last.x
                        } else {
                            p.y - last.y
                        };
                        self.ws.drag_handle(0, &h, d);
                    }
                    self.drag = Some((i, Some(p)));
                }
            }
            Msg::DragEnd => self.drag = None,
            Msg::Resized(id, size) => {
                if id == self.main {
                    self.size = size;
                }
            }
            Msg::Opened(_) => {}
            Msg::Closed(id) => {
                if id == self.main {
                    return iced::exit();
                }
                if let Some(i) = self.float_index(id) {
                    self.floats.remove(&id);
                    self.ws.dock(i);
                }
            }
        }
        self.sync_windows()
    }

    fn subscription(&self) -> Subscription<Msg> {
        let mut subs = vec![
            window::resize_events().map(|(id, s)| Msg::Resized(id, s)),
            window::close_events().map(Msg::Closed),
        ];
        // Frame ticks only while an animated panel is visible (ADR-0013).
        if self
            .ws
            .visible_panels()
            .iter()
            .any(|p| p == "rbs.program" || p == "rbs.mixer")
        {
            subs.push(window::frames().map(Msg::Tick));
        }
        // Pointer tracking only during a splitter drag.
        if self.drag.is_some() {
            subs.push(event::listen_with(|e, _, _| match e {
                Event::Mouse(mouse::Event::CursorMoved { position }) => {
                    Some(Msg::DragMove(position))
                }
                Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                    Some(Msg::DragEnd)
                }
                _ => None,
            }));
        }
        Subscription::batch(subs)
    }

    fn title(&self, id: window::Id) -> String {
        if id == self.main {
            "RBS prototype (iced)".into()
        } else {
            "RBS prototype (iced) - floating".into()
        }
    }

    fn view(&self, id: window::Id) -> Element<'_, Msg> {
        if id == self.main {
            let layout: Element<'_, Msg> = match self.ws.windows.first() {
                Some(w) => self.node(&w.layout, 0, &mut Vec::new(), &mut 0, &mut 0),
                None => text("").into(),
            };
            column![self.frame_bar(), layout].into()
        } else {
            let content: Element<'_, Msg> = match self
                .float_index(id)
                .and_then(|i| self.ws.windows.get(i).map(|w| (i, w)))
            {
                Some((i, w)) => self.node(&w.layout, i, &mut Vec::new(), &mut 0, &mut 0),
                None => text("").into(),
            };
            column![button("Dock back").on_press(Msg::Dock(id)), content].into()
        }
    }

    fn frame_bar(&self) -> Element<'_, Msg> {
        let live = container(text("LIVE 00:12:34 · 2 outputs").color(Color::WHITE))
            .padding([4, 8])
            .style(|_| container::Style {
                background: Some(Color::from_rgb8(0xe5, 0x48, 0x4d).into()),
                ..Default::default()
            });
        let mut r = row![
            live,
            text(format!("Workspace: {}", self.ws.name)),
            button("Edit").on_press(Msg::Switch("Edit")),
            button("Minimal").on_press(Msg::Switch("Minimal")),
            button("Save").on_press(Msg::Save),
            button("Load").on_press(Msg::Load),
            text("Panels:"),
        ]
        .spacing(6)
        .padding(4)
        .align_y(iced::Alignment::Center);
        for (id, title) in PANELS {
            let open = self.ws.is_open(id);
            let b = button(text(format!("{}{}", if open { "✓ " } else { "" }, title)))
                .on_press(Msg::Toggle((*id).into()))
                .style(if open {
                    button::primary
                } else {
                    button::secondary
                });
            r = r.push(b);
        }
        container(r).height(BAR).width(Length::Fill).into()
    }

    /// Build the layout tree recursively; `stack` and `handle` count in the same
    /// depth-first order as `dock_model::Workspace::flatten`.
    fn node<'a>(
        &'a self,
        n: &'a Node,
        win: usize,
        path: &mut Vec<usize>,
        stack: &mut usize,
        handle: &mut usize,
    ) -> Element<'a, Msg> {
        match n {
            Node::Tabs { tabs, active } => {
                let s = *stack;
                *stack += 1;
                self.stack_view(win, s, tabs, *active)
            }
            Node::Split {
                split,
                ratios,
                children,
            } => {
                let mut items: Vec<Element<'a, Msg>> = Vec::new();
                for (i, c) in children.iter().enumerate() {
                    path.push(i);
                    let child = self.node(c, win, path, stack, handle);
                    path.pop();
                    let portion = Length::FillPortion(
                        (ratios.get(i).copied().unwrap_or(0.5) * 1000.0).max(1.0) as u16,
                    );
                    let wrapped = if *split == Dir::Horizontal {
                        container(child).width(portion).height(Length::Fill)
                    } else {
                        container(child).height(portion).width(Length::Fill)
                    };
                    items.push(wrapped.into());
                    if i + 1 < children.len() {
                        let h = *handle;
                        *handle += 1;
                        let bar = container(text("")).style(|_| container::Style {
                            background: Some(Color::from_rgb8(0x16, 0x18, 0x1d).into()),
                            ..Default::default()
                        });
                        let bar = if *split == Dir::Horizontal {
                            bar.width(dock_model::HANDLE).height(Length::Fill)
                        } else {
                            bar.height(dock_model::HANDLE).width(Length::Fill)
                        };
                        let interaction = if *split == Dir::Horizontal {
                            mouse::Interaction::ResizingHorizontally
                        } else {
                            mouse::Interaction::ResizingVertically
                        };
                        let h_el = mouse_area(bar)
                            .on_press(Msg::DragStart(if win == 0 { h } else { usize::MAX }))
                            .interaction(interaction);
                        items.push(h_el.into());
                    }
                }
                if *split == Dir::Horizontal {
                    row(items).into()
                } else {
                    column(items).into()
                }
            }
        }
    }

    fn stack_view<'a>(
        &'a self,
        win: usize,
        s: usize,
        tabs: &'a [String],
        active: usize,
    ) -> Element<'a, Msg> {
        let mut bar = row![].spacing(2);
        for (i, id) in tabs.iter().enumerate() {
            let mut tab = row![
                button(text(dock_model::title(id)))
                    .on_press(Msg::Activate(win, s, i))
                    .style(if i == active {
                        button::primary
                    } else {
                        button::text
                    })
            ];
            if win == 0 {
                tab = tab.push(
                    button("→")
                        .on_press(Msg::MoveNext(id.clone()))
                        .style(button::text),
                );
                tab = tab.push(
                    button("⧉")
                        .on_press(Msg::Float(id.clone()))
                        .style(button::text),
                );
            }
            tab = tab.push(
                button("×")
                    .on_press(Msg::Close(id.clone()))
                    .style(button::text),
            );
            bar = bar.push(tab);
        }
        let content: Element<'a, Msg> = match tabs.get(active).map(|s| s.as_str()) {
            Some("rbs.program") => shader_w(ProgramView { t: self.t })
                .width(Length::Fill)
                .height(Length::Fill)
                .into(),
            Some("rbs.mixer") => canvas_w(SpectrumView {
                bands: &self.bands,
                cache: &self.spectrum,
            })
            .width(Length::Fill)
            .height(Length::Fill)
            .into(),
            Some("rbs.live-controls") => container(text(BIDI)).center(Length::Fill).into(),
            Some(other) => container(text(other.to_string()))
                .center(Length::Fill)
                .into(),
            None => text("").into(),
        };
        container(column![bar, content])
            .width(Length::Fill)
            .height(Length::Fill)
            .style(|_| container::Style {
                background: Some(Color::from_rgb8(0x20, 0x23, 0x2a).into()),
                ..Default::default()
            })
            .into()
    }
}

// ---- Program preview: engine texture shown through a custom wgpu primitive ----

struct ProgramView {
    t: f32,
}

#[derive(Debug)]
struct ProgramPrimitive {
    t: f32,
}

impl shader::Program<Msg> for ProgramView {
    type State = ();
    type Primitive = ProgramPrimitive;
    fn draw(&self, _: &(), _: mouse::Cursor, _: Rectangle) -> ProgramPrimitive {
        ProgramPrimitive { t: self.t }
    }
}

/// Per-renderer state. iced creates the device; the engine has to adopt it
/// (iced offers no way to hand it an existing device).
struct ProgramPipeline {
    engine: engine::Engine,
    blit: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    bind_group: Option<wgpu::BindGroup>,
}

impl shader::Pipeline for ProgramPipeline {
    fn new(device: &wgpu::Device, queue: &wgpu::Queue, format: wgpu::TextureFormat) -> Self {
        let engine = engine::Engine::new(device, queue, (1280, 720), wgpu::TextureUsages::empty());
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("blit"),
            source: wgpu::ShaderSource::Wgsl(include_str!("blit.wgsl").into()),
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: None,
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[&layout],
            ..Default::default()
        });
        let blit = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("blit"),
            layout: Some(&pl),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                targets: &[Some(format.into())],
                compilation_options: Default::default(),
            }),
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            multiview: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        ProgramPipeline {
            engine,
            blit,
            layout,
            sampler,
            bind_group: None,
        }
    }
}

impl shader::Primitive for ProgramPrimitive {
    type Pipeline = ProgramPipeline;

    fn prepare(
        &self,
        p: &mut ProgramPipeline,
        device: &wgpu::Device,
        _queue: &wgpu::Queue,
        _bounds: &Rectangle,
        _viewport: &Viewport,
    ) {
        let view = p.engine.render(self.t).create_view(&Default::default());
        p.bind_group = Some(device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &p.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&p.sampler),
                },
            ],
        }));
    }

    fn draw(&self, p: &ProgramPipeline, pass: &mut wgpu::RenderPass<'_>) -> bool {
        if let Some(bg) = &p.bind_group {
            pass.set_pipeline(&p.blit);
            pass.set_bind_group(0, bg, &[]);
            pass.draw(0..3, 0..1);
        }
        true
    }
}

// ---- Spectrum: drawn with the toolkit's own vector canvas ----

struct SpectrumView<'a> {
    bands: &'a [f32],
    cache: &'a canvas::Cache,
}

impl canvas::Program<Msg> for SpectrumView<'_> {
    type State = ();
    fn draw(
        &self,
        _: &(),
        renderer: &iced::Renderer,
        _: &Theme,
        bounds: Rectangle,
        _: mouse::Cursor,
    ) -> Vec<canvas::Geometry> {
        let g = self.cache.draw(renderer, bounds.size(), |f| {
            let n = self.bands.len().max(1) as f32;
            let bw = f.width() / n;
            for (i, v) in self.bands.iter().enumerate() {
                let h = v * f.height();
                let c = if *v > 0.85 {
                    Color::from_rgb8(0xe5, 0x48, 0x4d)
                } else {
                    Color::from_rgba8(0x4c, 0x8d, 0xff, 0.6)
                };
                f.fill_rectangle(
                    Point::new(i as f32 * bw, f.height() - h),
                    Size::new(bw - 1.0, h),
                    c,
                );
            }
            let line = Path::new(|b| {
                for (i, v) in self.bands.iter().enumerate() {
                    let p = Point::new(
                        f.width() * i as f32 / (n - 1.0).max(1.0),
                        f.height() * (1.0 - v),
                    );
                    if i == 0 { b.move_to(p) } else { b.line_to(p) }
                }
            });
            f.stroke(
                &line,
                Stroke::default().with_color(Color::WHITE).with_width(1.5),
            );
        });
        vec![g]
    }
}

fn main() -> iced::Result {
    iced::daemon(App::boot, App::update, App::view)
        .title(App::title)
        .subscription(App::subscription)
        .theme(|_: &App, _| Theme::Dark)
        .run()
}
