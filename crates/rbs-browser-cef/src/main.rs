//! `rbs-browser-cef`: the CEF tree root that replaces the test-double helper
//! (ADR-0109 decision 1, ADR-0132 decision 1).
//!
//! The studio starts it with the helper protocol of `rbs-browser-ipc`: one
//! `configure` message on stdin, frames in the studio-allocated section it names,
//! a heartbeat at least every 500 ms, and exit when stdin closes. The page boundary
//! of ADR-0109 decision 10 is applied here: no popups, downloads, file pickers,
//! JavaScript dialogs or device permissions, a `file` page reads only its own
//! directory, and the profile lives in a temporary directory removed on exit.
//!
//! The macOS and Windows code compiles from any host without the CEF binaries:
//! `cargo clippy --target aarch64-apple-darwin --features cef/dox` (or
//! `x86_64-pc-windows-msvc`), after `rustup target add` for that target.
//!
//! Page audio never reaches an OS device (ADR-0132 decision 7). When the studio names a
//! sound section, each packet goes there with the OS monotonic time of its first frame
//! (ADR-0109 decision 6); otherwise it is dropped.
//!
//! Not yet: the GPU texture path, CSS, reload, input and the sandbox (ADR-0132 steps).

use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use cef::*;
use rbs_browser_ipc::page::{bgra_premultiplied_to_rgba, file_scope, request_allowed};
use rbs_browser_ipc::{Configure, HEARTBEAT_PERIOD, MAX_PAYLOAD, decode_configure};
use rbs_browser_shm::{
    FrameRing, MAX_SOUND_CHANNELS, RingWriter, SlotLayout, SoundRing, os_monotonic_nanos,
};

/// Exit codes the studio may log. Any exit is Unavailable to the studio.
mod exit {
    pub const BAD_CONFIGURE: i32 = 2;
    pub const NO_SECTION: i32 = 3;
    pub const INIT_FAILED: i32 = 4;
    pub const NO_BROWSER: i32 = 5;
    pub const RENDERER_GONE: i32 = 6;
}

/// Everything the CEF callbacks share. CEF calls them on its UI thread, which is
/// the thread that runs `main`'s message loop.
struct Page {
    ring: FrameRing,
    writer: Mutex<RingWriter>,
    transparent: bool,
    scope: Option<String>,
    /// The renderer stopped answering: stop beating so the studio sees a hang.
    unresponsive: AtomicBool,
    /// Set when the browser is gone or the renderer died; the loop ends.
    quit: AtomicBool,
    /// Exit code chosen by the event that ended the loop.
    code: AtomicI32,
    closed: AtomicBool,
    /// The studio's sound section; `None`: page audio is dropped.
    sound: Option<SoundRing>,
    /// The rate the studio asked for (0: the engine's own).
    sample_rate: u32,
    /// Rate and channels of the current audio stream (CEF's audio thread).
    stream_rate: AtomicU32,
    stream_channels: AtomicU32,
}

static PAGE: OnceLock<Page> = OnceLock::new();

fn page() -> Option<&'static Page> {
    PAGE.get()
}

wrap_app! {
    struct RunnerApp;
    impl App {
        fn on_before_command_line_processing(
            &self,
            _process_type: Option<&CefString>,
            command_line: Option<&mut CommandLine>,
        ) {
            let Some(line) = command_line else { return };
            // A page plays sound without a click; the sound is captured, never
            // played by the OS (ADR-0132 decision 7).
            line.append_switch_with_value(
                Some(&"autoplay-policy".into()),
                Some(&"no-user-gesture-required".into()),
            );
            // Software paint into the shared section. The GPU path is a later step.
            line.append_switch(Some(&"disable-gpu".into()));
            line.append_switch(Some(&"disable-gpu-compositing".into()));
            line.append_switch(Some(&"deny-permission-prompts".into()));
            // The helper never shows a window, so it needs no X11 or Wayland
            // display on Linux.
            #[cfg(target_os = "linux")]
            line.append_switch_with_value(
                Some(&"ozone-platform".into()),
                Some(&"headless".into()),
            );
            line.append_switch(Some(&"disable-background-networking".into()));
            line.append_switch(Some(&"disable-sync".into()));
            line.append_switch(Some(&"disable-component-update".into()));
            line.append_switch(Some(&"no-first-run".into()));
            line.append_switch(Some(&"use-mock-keychain".into()));
            line.append_switch_with_value(
                Some(&"password-store".into()),
                Some(&"basic".into()),
            );
        }
    }
}

wrap_render_handler! {
    struct Paint;
    impl RenderHandler {
        fn view_rect(&self, _browser: Option<&mut Browser>, rect: Option<&mut Rect>) {
            let (Some(rect), Some(page)) = (rect, page()) else { return };
            rect.x = 0;
            rect.y = 0;
            rect.width = i32::try_from(page.ring.width()).unwrap_or(i32::MAX);
            rect.height = i32::try_from(page.ring.height()).unwrap_or(i32::MAX);
        }

        fn on_paint(
            &self,
            _browser: Option<&mut Browser>,
            type_: PaintElementType,
            _dirty_rects: Option<&[Rect]>,
            buffer: *const u8,
            width: ::std::os::raw::c_int,
            height: ::std::os::raw::c_int,
        ) {
            let Some(page) = page() else { return };
            if type_ != PaintElementType::VIEW || buffer.is_null() {
                return;
            }
            // During a resize CEF may paint the old size once; skip it.
            if u32::try_from(width).ok() != Some(page.ring.width())
                || u32::try_from(height).ok() != Some(page.ring.height())
            {
                return;
            }
            let len = page.ring.width() as usize * page.ring.height() as usize * 4;
            // SAFETY: CEF passes a `width * height * 4` byte BGRA buffer that stays
            // valid for the duration of this callback; the size was checked equal to
            // the section's slot size above, and the slice does not outlive the call.
            let pixels = unsafe { std::slice::from_raw_parts(buffer, len) };
            let Ok(mut writer) = page.writer.lock() else { return };
            let transparent = page.transparent;
            page.ring.publish_with(&mut writer, |slot| match page.ring.layout() {
                SlotLayout::Bgra8Premultiplied => slot.copy_from_slice(pixels),
                // The studio's current upload path takes straight Rgba8. The helper
                // converts while it copies (the copy happens anyway), so the studio
                // adds no per-frame pass. The GPU path replaces this.
                SlotLayout::Rgba8Straight => bgra_premultiplied_to_rgba(pixels, slot, transparent),
            });
        }
    }
}

wrap_audio_handler! {
    struct Sound;
    impl AudioHandler {
        // Returning 1 routes page audio here instead of the OS output (ADR-0132
        // decision 7). With a sound section, ask for the studio's rate in stereo,
        // in 10 ms packets.
        fn audio_parameters(
            &self,
            _browser: Option<&mut Browser>,
            params: Option<&mut AudioParameters>,
        ) -> ::std::os::raw::c_int {
            if let (Some(page), Some(params)) = (page(), params)
                && page.sound.is_some()
                && let Ok(rate) = i32::try_from(page.sample_rate)
                && rate > 0
            {
                params.sample_rate = rate;
                params.channel_layout = ChannelLayout::LAYOUT_STEREO;
                params.frames_per_buffer = rate / 100;
            }
            1
        }

        fn on_audio_stream_started(
            &self,
            _browser: Option<&mut Browser>,
            params: Option<&AudioParameters>,
            channels: ::std::os::raw::c_int,
        ) {
            let Some(page) = page() else {
                return;
            };
            let rate = params.map_or(0, |p| u32::try_from(p.sample_rate).unwrap_or(0));
            page.stream_rate.store(rate, Ordering::Release);
            page.stream_channels
                .store(u32::try_from(channels).unwrap_or(0), Ordering::Release);
        }

        fn on_audio_stream_packet(
            &self,
            _browser: Option<&mut Browser>,
            data: *mut *const f32,
            frames: ::std::os::raw::c_int,
            _pts: i64,
        ) {
            let Some(page) = page() else {
                return;
            };
            let Some(sound) = page.sound.as_ref() else {
                return;
            };
            let rate = page.stream_rate.load(Ordering::Acquire);
            let channels = page.stream_channels.load(Ordering::Acquire) as usize;
            let Ok(frames) = usize::try_from(frames) else {
                return;
            };
            if data.is_null() || rate == 0 || channels == 0 || frames == 0 {
                return;
            }
            // The packet was just made: its first frame is one packet ago.
            let duration = frames as u64 * 1_000_000_000 / u64::from(rate);
            let os_nanos = os_monotonic_nanos().saturating_sub(duration);
            let mut planes: [&[f32]; MAX_SOUND_CHANNELS] = [&[]; MAX_SOUND_CHANNELS];
            let used = channels.min(MAX_SOUND_CHANNELS);
            for (c, plane) in planes.iter_mut().enumerate().take(used) {
                // SAFETY: CEF passes `channels` (from `on_audio_stream_started`)
                // plane pointers, each to `frames` floats, valid for this call;
                // `c < channels`.
                let ptr = unsafe { *data.add(c) };
                if ptr.is_null() {
                    return;
                }
                // SAFETY: as above; the slice lives only inside this callback.
                *plane = unsafe { std::slice::from_raw_parts(ptr, frames) };
            }
            sound.write_planar(os_nanos, rate, &planes[..used], frames);
        }
    }
}

wrap_life_span_handler! {
    struct Life;
    impl LifeSpanHandler {
        fn on_before_popup(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _popup_id: ::std::os::raw::c_int,
            _target_url: Option<&CefString>,
            _target_frame_name: Option<&CefString>,
            _target_disposition: WindowOpenDisposition,
            _user_gesture: ::std::os::raw::c_int,
            _popup_features: Option<&PopupFeatures>,
            _window_info: Option<&mut WindowInfo>,
            _client: Option<&mut Option<Client>>,
            _settings: Option<&mut BrowserSettings>,
            _extra_info: Option<&mut Option<DictionaryValue>>,
            _no_javascript_access: Option<&mut ::std::os::raw::c_int>,
        ) -> ::std::os::raw::c_int {
            1
        }

        fn on_before_close(&self, _browser: Option<&mut Browser>) {
            if let Some(page) = page() {
                page.closed.store(true, Ordering::Release);
                page.quit.store(true, Ordering::Release);
            }
        }
    }
}

wrap_resource_request_handler! {
    struct Resources;
    impl ResourceRequestHandler {
        fn on_before_resource_load(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            request: Option<&mut Request>,
            _callback: Option<&mut Callback>,
        ) -> ReturnValue {
            let (Some(request), Some(page)) = (request, page()) else {
                return ReturnValue::CANCEL;
            };
            let url = CefString::from(&request.url()).to_string();
            if request_allowed(page.scope.as_deref(), &url) {
                ReturnValue::CONTINUE
            } else {
                ReturnValue::CANCEL
            }
        }
    }
}

wrap_request_handler! {
    struct Requests {
        resources: ResourceRequestHandler,
    }
    impl RequestHandler {
        fn resource_request_handler(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _request: Option<&mut Request>,
            _is_navigation: ::std::os::raw::c_int,
            _is_download: ::std::os::raw::c_int,
            _request_initiator: Option<&CefString>,
            _disable_default_handling: Option<&mut ::std::os::raw::c_int>,
        ) -> Option<ResourceRequestHandler> {
            Some(self.resources.clone())
        }

        fn on_render_process_unresponsive(
            &self,
            _browser: Option<&mut Browser>,
            _callback: Option<&mut UnresponsiveProcessCallback>,
        ) -> ::std::os::raw::c_int {
            if let Some(page) = page() {
                page.unresponsive.store(true, Ordering::Release);
            }
            // Keep CEF's default (wait); the studio decides on the lost heartbeat.
            0
        }

        fn on_render_process_responsive(&self, _browser: Option<&mut Browser>) {
            if let Some(page) = page() {
                page.unresponsive.store(false, Ordering::Release);
            }
        }

        fn on_render_process_terminated(
            &self,
            _browser: Option<&mut Browser>,
            _status: TerminationStatus,
            _error_code: ::std::os::raw::c_int,
            _error_string: Option<&CefString>,
        ) {
            if let Some(page) = page() {
                page.code.store(exit::RENDERER_GONE, Ordering::Release);
                page.quit.store(true, Ordering::Release);
            }
        }
    }
}

wrap_permission_handler! {
    struct Permissions;
    impl PermissionHandler {
        fn on_request_media_access_permission(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _requesting_origin: Option<&CefString>,
            _requested_permissions: u32,
            callback: Option<&mut MediaAccessCallback>,
        ) -> ::std::os::raw::c_int {
            if let Some(callback) = callback {
                callback.cancel();
            }
            1
        }

        fn on_show_permission_prompt(
            &self,
            _browser: Option<&mut Browser>,
            _prompt_id: u64,
            _requesting_origin: Option<&CefString>,
            _requested_permissions: u32,
            callback: Option<&mut PermissionPromptCallback>,
        ) -> ::std::os::raw::c_int {
            if let Some(callback) = callback {
                callback.cont(PermissionRequestResult::DENY);
            }
            1
        }
    }
}

wrap_jsdialog_handler! {
    struct Dialogs;
    impl JsdialogHandler {
        fn on_jsdialog(
            &self,
            _browser: Option<&mut Browser>,
            _origin_url: Option<&CefString>,
            _dialog_type: JsdialogType,
            _message_text: Option<&CefString>,
            _default_prompt_text: Option<&CefString>,
            _callback: Option<&mut JsdialogCallback>,
            suppress_message: Option<&mut ::std::os::raw::c_int>,
        ) -> ::std::os::raw::c_int {
            if let Some(suppress) = suppress_message {
                *suppress = 1;
            }
            0
        }

        fn on_before_unload_dialog(
            &self,
            _browser: Option<&mut Browser>,
            _message_text: Option<&CefString>,
            _is_reload: ::std::os::raw::c_int,
            callback: Option<&mut JsdialogCallback>,
        ) -> ::std::os::raw::c_int {
            // Leaving the page is always allowed: closing must never wait on it.
            if let Some(callback) = callback {
                callback.cont(1, None);
            }
            1
        }
    }
}

wrap_download_handler! {
    struct Downloads;
    impl DownloadHandler {
        fn can_download(
            &self,
            _browser: Option<&mut Browser>,
            _url: Option<&CefString>,
            _request_method: Option<&CefString>,
        ) -> ::std::os::raw::c_int {
            0
        }
    }
}

wrap_dialog_handler! {
    struct FileDialogs;
    impl DialogHandler {
        fn on_file_dialog(
            &self,
            _browser: Option<&mut Browser>,
            _mode: FileDialogMode,
            _title: Option<&CefString>,
            _default_file_path: Option<&CefString>,
            _accept_filters: Option<&mut CefStringList>,
            _accept_extensions: Option<&mut CefStringList>,
            _accept_descriptions: Option<&mut CefStringList>,
            callback: Option<&mut FileDialogCallback>,
        ) -> ::std::os::raw::c_int {
            if let Some(callback) = callback {
                callback.cancel();
            }
            1
        }
    }
}

wrap_client! {
    struct RunnerClient {
        paint: RenderHandler,
        sound: AudioHandler,
        life: LifeSpanHandler,
        requests: RequestHandler,
    }
    impl Client {
        fn render_handler(&self) -> Option<RenderHandler> {
            Some(self.paint.clone())
        }
        fn audio_handler(&self) -> Option<AudioHandler> {
            Some(self.sound.clone())
        }
        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(self.life.clone())
        }
        fn request_handler(&self) -> Option<RequestHandler> {
            Some(self.requests.clone())
        }
        fn permission_handler(&self) -> Option<PermissionHandler> {
            Some(Permissions::new())
        }
        fn jsdialog_handler(&self) -> Option<JsdialogHandler> {
            Some(Dialogs::new())
        }
        fn download_handler(&self) -> Option<DownloadHandler> {
            Some(Downloads::new())
        }
        fn dialog_handler(&self) -> Option<DialogHandler> {
            Some(FileDialogs::new())
        }
    }
}

/// Reads the length-prefixed `configure` message.
fn read_configure(input: &mut impl Read) -> Option<Configure> {
    let mut len_buf = [0u8; 4];
    input.read_exact(&mut len_buf).ok()?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > MAX_PAYLOAD {
        return None;
    }
    let mut message = vec![0u8; 4 + len];
    message[..4].copy_from_slice(&len_buf);
    input.read_exact(&mut message[4..]).ok()?;
    decode_configure(&message)
}

#[cfg(target_os = "macos")]
mod mac {
    //! CEF needs the browser process's `NSApplication` to implement its protocols.

    use std::cell::Cell;

    use cef::application_mac::{CefAppProtocol, CrAppControlProtocol, CrAppProtocol};
    use objc2::runtime::Bool;
    use objc2::{ClassType, DefinedClass, define_class, extern_methods, msg_send, rc::Retained};
    use objc2_app_kit::{NSApp, NSApplication, NSEvent};
    use objc2_foundation::{MainThreadMarker, NSObjectProtocol};

    #[derive(Default)]
    pub struct Ivars {
        handling: Cell<Bool>,
    }

    define_class!(
        #[unsafe(super(NSApplication))]
        #[ivars = Ivars]
        pub struct RunnerApplication;

        impl RunnerApplication {
            #[unsafe(method(sendEvent:))]
            unsafe fn send_event(&self, event: &NSEvent) {
                let was = self.ivars().handling.get();
                self.ivars().handling.set(Bool::YES);
                // SAFETY: forwards the event to NSApplication's own sendEvent:.
                let _: () = unsafe { msg_send![super(self), sendEvent: event] };
                self.ivars().handling.set(was);
            }
        }

        unsafe impl CrAppControlProtocol for RunnerApplication {
            #[unsafe(method(setHandlingSendEvent:))]
            unsafe fn set_handling(&self, value: Bool) {
                self.ivars().handling.set(value);
            }
        }

        unsafe impl CrAppProtocol for RunnerApplication {
            #[unsafe(method(isHandlingSendEvent))]
            unsafe fn is_handling(&self) -> Bool {
                self.ivars().handling.get()
            }
        }

        unsafe impl CefAppProtocol for RunnerApplication {}
    );

    impl RunnerApplication {
        extern_methods! {
            #[unsafe(method(sharedApplication))]
            fn shared_application() -> Retained<Self>;
        }
    }

    /// Makes the shared application our subclass. False when that failed.
    pub fn setup() -> bool {
        let _ = RunnerApplication::shared_application();
        MainThreadMarker::new()
            .is_some_and(|mtm| NSApp(mtm).isKindOfClass(RunnerApplication::class()))
    }
}

fn main() {
    #[cfg(target_os = "macos")]
    let _loader = {
        let Ok(exe) = std::env::current_exe() else {
            std::process::exit(exit::INIT_FAILED);
        };
        let loader = library_loader::LibraryLoader::new(&exe, false);
        if !loader.load() {
            std::process::exit(exit::INIT_FAILED);
        }
        loader
    };
    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);
    let args = args::Args::new();
    let mut app = RunnerApp::new();
    // Renderer, GPU and utility processes on Linux and Windows run this
    // executable and return here.
    let code = execute_process(
        Some(args.as_main_args()),
        Some(&mut app),
        std::ptr::null_mut(),
    );
    if code >= 0 {
        std::process::exit(code);
    }
    std::process::exit(run_browser(&args, &mut app));
}

fn run_browser(args: &args::Args, app: &mut App) -> i32 {
    #[cfg(target_os = "macos")]
    if !mac::setup() {
        return exit::INIT_FAILED;
    }
    let mut stdin = std::io::stdin();
    let Some(config) = read_configure(&mut stdin) else {
        return exit::BAD_CONFIGURE;
    };
    let Ok(ring) = FrameRing::open(&config.frames, config.width, config.height) else {
        return exit::NO_SECTION;
    };
    let writer = ring.writer();
    let scope = file_scope(&config.url).map(str::to_owned);
    // No sound section, or one that does not open: the page is silent to the studio.
    let sound = (!config.sound.is_empty())
        .then(|| SoundRing::open(&config.sound).ok())
        .flatten();
    let page = PAGE.get_or_init(|| Page {
        ring,
        writer: Mutex::new(writer),
        transparent: config.transparent,
        scope,
        unresponsive: AtomicBool::new(false),
        quit: AtomicBool::new(false),
        code: AtomicI32::new(0),
        closed: AtomicBool::new(false),
        sound,
        sample_rate: config.sample_rate,
        stream_rate: AtomicU32::new(0),
        stream_channels: AtomicU32::new(0),
    });

    // Stdin closing is the studio's stop (ADR-0109 decision 8).
    let stop = std::sync::Arc::new(AtomicBool::new(false));
    let stop_flag = std::sync::Arc::clone(&stop);
    let _ = std::thread::Builder::new()
        .name("rbs-browser-stdin".into())
        .spawn(move || {
            let mut rest = [0u8; 64];
            while matches!(stdin.read(&mut rest), Ok(n) if n > 0) {}
            stop_flag.store(true, Ordering::Release);
        });

    // One temporary profile per tree, removed on exit (ADR-0109 decision 10).
    let Some(profile) = new_profile_dir() else {
        return exit::INIT_FAILED;
    };
    let settings = Settings {
        windowless_rendering_enabled: 1,
        // The sandbox is a later step (ADR-0132); until then this runner is opt-in.
        no_sandbox: 1,
        root_cache_path: profile.to_string_lossy().as_ref().into(),
        persist_session_cookies: 0,
        log_severity: LogSeverity::ERROR,
        log_file: profile.join("cef.log").to_string_lossy().as_ref().into(),
        ..Default::default()
    };
    if initialize(
        Some(args.as_main_args()),
        Some(&settings),
        Some(app),
        std::ptr::null_mut(),
    ) != 1
    {
        remove_profile(&profile);
        return exit::INIT_FAILED;
    }

    let code = browse(page, &config, &stop);
    shutdown();
    remove_profile(&profile);
    code
}

/// How long [`remove_profile`] keeps trying. Below the studio's stop grace together
/// with a normal close, so the studio does not kill the tree in the middle of it.
const PROFILE_REMOVE_LIMIT: std::time::Duration = std::time::Duration::from_millis(600);

/// Removes the tree's profile (ADR-0109 decision 10). CEF's GPU and renderer processes
/// can still hold files in it for a moment after `shutdown`, and Windows does not
/// delete an open file, so a failed removal is tried again until
/// [`PROFILE_REMOVE_LIMIT`].
fn remove_profile(profile: &std::path::Path) {
    let deadline = std::time::Instant::now() + PROFILE_REMOVE_LIMIT;
    loop {
        match std::fs::remove_dir_all(profile) {
            Ok(()) => return,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(_) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(_) => return,
        }
    }
}

/// A new private directory for this tree's profile. Created here, never reused, so
/// another user cannot plant it in a shared temporary directory.
fn new_profile_dir() -> Option<std::path::PathBuf> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.subsec_nanos());
    for attempt in 0..8u32 {
        let path = std::env::temp_dir().join(format!(
            "rbs-browser-{}-{nanos:x}-{attempt}",
            std::process::id()
        ));
        #[cfg(unix)]
        let builder = {
            let mut builder = std::fs::DirBuilder::new();
            std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
            builder
        };
        #[cfg(not(unix))]
        let builder = std::fs::DirBuilder::new();
        if builder.create(&path).is_ok() {
            return Some(path);
        }
    }
    None
}

fn browse(page: &Page, config: &Configure, stop: &AtomicBool) -> i32 {
    let window = WindowInfo::default().set_as_windowless(Default::default());
    let browser_settings = BrowserSettings {
        windowless_frame_rate: i32::try_from(config.fps.clamp(1, 60)).unwrap_or(30),
        // 0 is fully transparent; otherwise an opaque black page background.
        background_color: if config.transparent { 0 } else { 0xFF00_0000 },
        javascript_access_clipboard: State::DISABLED,
        javascript_dom_paste: State::DISABLED,
        ..Default::default()
    };
    let mut client = RunnerClient::new(
        Paint::new(),
        Sound::new(),
        Life::new(),
        Requests::new(Resources::new()),
    );
    let Some(browser) = browser_host_create_browser_sync(
        Some(&window),
        Some(&mut client),
        Some(&config.url.as_str().into()),
        Some(&browser_settings),
        None,
        None,
    ) else {
        return exit::NO_BROWSER;
    };

    // Pump CEF at least twice per page frame, and often enough for the heartbeat.
    let period = Duration::from_secs(1) / config.fps.clamp(1, 60);
    let pump = (period / 2).min(Duration::from_millis(10));
    let mut last_beat = Instant::now();
    page.ring.beat();
    while !page.quit.load(Ordering::Acquire) && !stop.load(Ordering::Acquire) {
        do_message_loop_work();
        if last_beat.elapsed() >= HEARTBEAT_PERIOD / 2 && !page.unresponsive.load(Ordering::Acquire)
        {
            page.ring.beat();
            last_beat = Instant::now();
        }
        std::thread::sleep(pump);
    }

    if !page.closed.load(Ordering::Acquire) {
        if let Some(host) = browser.host() {
            host.close_browser(1);
        }
        // Closing is quick; never hang the exit on a stuck page.
        let deadline = Instant::now() + Duration::from_secs(2);
        while !page.closed.load(Ordering::Acquire) && Instant::now() < deadline {
            do_message_loop_work();
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    drop(browser);
    page.code.load(Ordering::Acquire)
}
