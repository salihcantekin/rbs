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
//! The CEF runtime is not shipped with the runner: it is the browser engine add-on,
//! loaded from the directory the studio names (`engine`, ADR-0145).
//!
//! After `configure`, the studio sends control messages on the same pipe
//! (`rbs_browser_ipc::control`, ADR-0132 decision 8): reload and run the script. The
//! page settings of ADR-0132 and ADR-0149 (CSS, zoom, the user's script, periodic
//! reload, retry on load errors, reload when a `file` page's directory changes, a kept
//! profile) arrive in `configure`; the load cycle that holds frames and times the
//! reloads, retries and script runs is `rbs_browser_ipc::automation::Cycle`.
//!
//! Not yet: the GPU texture path, input, the status channel back to the studio and the
//! sandbox (ADR-0132 steps).

use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use cef::*;
use rbs_browser_ipc::automation::{
    Cycle, CycleOptions, Due, Start, css_script, file_dir, page_css, script_may_run, zoom_level,
};
use rbs_browser_ipc::control::Control;
use rbs_browser_ipc::page::{bgra_premultiplied_to_rgba, file_scope, request_allowed};
use rbs_browser_ipc::{Configure, HEARTBEAT_PERIOD, MAX_PAYLOAD, PageOptions, decode_configure};
use rbs_browser_shm::{
    FrameRing, MAX_SOUND_CHANNELS, RingWriter, SlotLayout, SoundRing, os_monotonic_nanos,
};

mod engine;
mod input;

// Exit codes, numbered in `rbs-browser-ipc`. Any exit is Unavailable to the studio;
// the runtime ones name why (add-ons spec "Browser engine consumer").
use rbs_browser_ipc::exit;

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
    /// The saved address: the script runs only on its origin (ADR-0149 decision 3).
    url: String,
    /// CSS, zoom, script and timers (ADR-0132, ADR-0149); the studio changes them
    /// while the page runs (ADR-0149 decision 1).
    options: Mutex<PageOptions>,
    /// The style sheet added at each commit: the transparent-background rule and
    /// the user's CSS (`page_css`); empty for none.
    css: Mutex<String>,
    /// The page has the engine's input focus (set at the first input event).
    focused: AtomicBool,
    /// The main frame's load cycle: held frames, reloads, retries, script runs.
    cycle: Mutex<Cycle>,
    /// The runner started the next main-frame navigation itself (a reload or a
    /// retry) and already told `cycle`.
    ours: AtomicBool,
    /// The runner's clock for `cycle`.
    epoch: Instant,
    /// The paint rate the studio asked for; a hidden background page paints once per
    /// second instead (ADR-0149 decision 5).
    fps: i32,
}

impl Page {
    fn now(&self) -> Duration {
        self.epoch.elapsed()
    }

    fn zoom(&self) -> u16 {
        self.options.lock().map_or(100, |o| o.zoom_percent)
    }

    fn script(&self) -> String {
        self.options
            .lock()
            .map(|o| o.script.clone())
            .unwrap_or_default()
    }

    fn css(&self) -> String {
        self.css.lock().map(|c| c.clone()).unwrap_or_default()
    }

    fn with_cycle<T>(&self, f: impl FnOnce(&mut Cycle) -> T) -> Option<T> {
        self.cycle.lock().ok().map(|mut cycle| f(&mut cycle))
    }

    /// Runs `script` in `frame` when the frame is on the saved origin. Returns
    /// whether it was sent (ADR-0149 decision 3: no result, no error report).
    fn run_script(&self, frame: &Frame, script: &str) -> bool {
        if script.is_empty() {
            return false;
        }
        let current = CefString::from(&frame.url()).to_string();
        if !script_may_run(&self.url, &current) {
            return false;
        }
        frame.execute_java_script(Some(&script.into()), None, 0);
        true
    }
}

static PAGE: OnceLock<Page> = OnceLock::new();

/// The studio adapter's LUID while this tree copies CEF's texture into the studio's
/// (Windows, ADR-0157 decision 8); 0 for software paint.
#[cfg(windows)]
static GPU_LUID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
#[cfg(windows)]
static COPIER: OnceLock<gpu::Copier> = OnceLock::new();
/// Counted copy failures since the last published picture (ADR-0157 decision 6).
#[cfg(windows)]
static GPU_FAILS: AtomicU32 = AtomicU32::new(0);
/// A callback published nothing: the loop asks CEF for one new view paint.
#[cfg(windows)]
static GPU_REPAINT: AtomicBool = AtomicBool::new(false);
/// OS monotonic nanoseconds of the first paint that found no free GPU slot since the
/// last one that did; 0 while slots are free (ADR-0157 decision 5).
#[cfg(windows)]
static NO_SLOT_SINCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Counted failures after which the tree falls back to shared memory (`copy`).
#[cfg(windows)]
const GPU_FAIL_LIMIT: u32 = 60;

#[cfg(windows)]
mod gpu;

/// The `--use-adapter-luid` value for `luid`: Chromium parses decimal `<high>,<low>`
/// with a signed high part and an unsigned low part (`ui/gl/gl_display.cc`); hex fails
/// and makes the GPU process exit (seen on CEF 156, ADR-0157 decision 8).
fn adapter_luid_switch(luid: u64) -> String {
    let high = (luid >> 32) as u32 as i32;
    let low = luid as u32;
    format!("{high},{low}")
}

fn page() -> Option<&'static Page> {
    PAGE.get()
}

wrap_app! {
    struct RunnerApp;
    impl App {
        fn on_before_command_line_processing(
            &self,
            process_type: Option<&CefString>,
            command_line: Option<&mut CommandLine>,
        ) {
            let Some(line) = command_line else { return };
            // A page plays sound without a click; the sound is captured, never
            // played by the OS (ADR-0132 decision 7).
            line.append_switch_with_value(
                Some(&"autoplay-policy".into()),
                Some(&"no-user-gesture-required".into()),
            );
            // The GPU switches are the browser process's choice alone; Chromium hands
            // the GPU and renderer processes what they need. This callback runs in
            // every process, and a child cannot tell the GPU path from shared memory:
            // a renderer that added `disable-gpu-compositing` under a GPU-composited
            // browser left CEF 156 with no paint at all, accelerated or not (SC-PC
            // 2026-11-20, ADR-0157 decision 8).
            let browser_process = process_type.is_none_or(|t| t.to_string().is_empty());
            #[cfg(windows)]
            let luid = GPU_LUID.load(Ordering::Relaxed);
            #[cfg(not(windows))]
            let luid = 0;
            let pin = std::env::var_os("RBS_BROWSER_NO_ADAPTER_SWITCH").is_none();
            for (name, value) in gpu_switches(browser_process, luid, pin) {
                match value {
                    Some(value) => line.append_switch_with_value(
                        Some(&name.into()),
                        Some(&value.as_str().into()),
                    ),
                    None => line.append_switch(Some(&name.into())),
                }
            }
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

/// The GPU switches for one CEF process (ADR-0157 decision 8). Only the browser
/// process gets any: software paint (`disable-gpu`, `disable-gpu-compositing`)
/// unless the tree was given studio textures on adapter `luid`, and then CEF's GPU
/// process pinned to that adapter when `pin` is set. A child process gets none:
/// it cannot tell the GPU path from shared memory, and a renderer that turned GPU
/// compositing off under a GPU-composited browser left CEF 156 painting nothing
/// (SC-PC check 3). Chromium hands the children what they need.
fn gpu_switches(
    browser_process: bool,
    luid: u64,
    pin: bool,
) -> Vec<(&'static str, Option<String>)> {
    if !browser_process {
        return Vec::new();
    }
    if luid == 0 {
        return vec![("disable-gpu", None), ("disable-gpu-compositing", None)];
    }
    if pin {
        return vec![("use-adapter-luid", Some(adapter_luid_switch(luid)))];
    }
    Vec::new()
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

        fn screen_info(
            &self,
            _browser: Option<&mut Browser>,
            screen_info: Option<&mut ScreenInfo>,
        ) -> ::std::os::raw::c_int {
            // Scale 1 keeps the painted bitmap the same size as the studio texture.
            // A monitor scale paints a larger texture; copying that into the smaller
            // slot does nothing and the page stays black.
            let (Some(info), Some(page)) = (screen_info, page()) else {
                return 0;
            };
            let width = i32::try_from(page.ring.width()).unwrap_or(0);
            let height = i32::try_from(page.ring.height()).unwrap_or(0);
            info.device_scale_factor = 1.0;
            info.depth = 24;
            info.depth_per_component = 8;
            info.is_monochrome = 0;
            info.rect.x = 0;
            info.rect.y = 0;
            info.rect.width = width;
            info.rect.height = height;
            info.available_rect = info.rect.clone();
            1
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
            // A document still held (no status, style or zoom yet, or a failed
            // load) is not sent; the studio keeps the last good picture.
            if !page.with_cycle(|c| c.painted(page.now())).unwrap_or(true) {
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

        #[cfg(windows)]
        fn on_accelerated_paint(
            &self,
            _browser: Option<&mut Browser>,
            type_: PaintElementType,
            _dirty_rects: Option<&[Rect]>,
            info: Option<&AcceleratedPaintInfo>,
        ) {
            if type_ != PaintElementType::VIEW {
                return;
            }
            let (Some(info), Some(page), Some(copier)) = (info, page(), COPIER.get()) else {
                return;
            };
            // CEF's handle is valid for this callback. A panic here would abort the
            // helper (ADR-0055); the copy counts as failed instead.
            let handle = info.shared_texture_handle;
            let copied = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                copier.copy(handle, &page.ring)
            }))
            .unwrap_or(gpu::Copied::Failed);
            // ADR-0157 decision 6: a published picture clears the count, no free
            // slot leaves it, a busy mutex or a failed copy adds one; a failed open
            // of CEF's texture means another adapter (decision 8).
            if copied == gpu::Copied::NoSlot {
                let _ = NO_SLOT_SINCE.compare_exchange(
                    0,
                    os_monotonic_nanos().max(1),
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                );
            } else {
                NO_SLOT_SINCE.store(0, Ordering::Relaxed);
            }
            let reason = match copied {
                gpu::Copied::Published => {
                    GPU_FAILS.store(0, Ordering::Relaxed);
                    None
                }
                gpu::Copied::NoSlot => None,
                gpu::Copied::Busy | gpu::Copied::Failed => {
                    (GPU_FAILS.fetch_add(1, Ordering::Relaxed) + 1 >= GPU_FAIL_LIMIT)
                        .then_some("copy")
                }
                gpu::Copied::OpenFailed => Some("open"),
            };
            if copied != gpu::Copied::Published {
                GPU_REPAINT.store(true, Ordering::Relaxed);
            }
            if let Some(reason) = reason {
                eprintln!("rbs-browser-cef: GPU path falls back to shared memory ({reason})");
                page.code.store(exit::GPU_FALLBACK, Ordering::Release);
                page.quit.store(true, Ordering::Release);
            }
        }
    }
}

/// Paints without a free GPU slot this long are logged (ADR-0157 decision 5): a lease
/// the studio holds too long, or a leaked one, becomes visible.
const SLOT_SHORTAGE_LOG: u64 = 1_000_000_000;

/// The loop's view of a shortage of free GPU slots: one line when it has lasted
/// [`SLOT_SHORTAGE_LOG`], one when a slot is free again. Short shortages, which a late
/// GPU causes (decision 5), are not logged.
#[derive(Debug, Default)]
#[cfg_attr(not(windows), allow(dead_code))]
struct SlotShortage {
    warned: bool,
    since: u64,
}

#[cfg_attr(not(windows), allow(dead_code))]
impl SlotShortage {
    /// `since` is the shortage's start (0: none), `now` the same clock.
    fn check(&mut self, since: u64, now: u64) -> Option<String> {
        if since != 0 {
            self.since = since;
            if !self.warned && now.saturating_sub(since) >= SLOT_SHORTAGE_LOG {
                self.warned = true;
                return Some("no free GPU slot for 1 s: a studio lease is held long".into());
            }
            return None;
        }
        if !self.warned {
            return None;
        }
        self.warned = false;
        let ms = now.saturating_sub(self.since) / 1_000_000;
        Some(format!("a GPU slot is free again after about {ms} ms"))
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
    struct MainResponses;
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

        fn on_resource_response(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            request: Option<&mut Request>,
            response: Option<&mut Response>,
        ) -> ::std::os::raw::c_int {
            // The main frame's status, before its document commits (ADR-0149
            // decision 6). A failed one stays held; the cycle retries it.
            let (Some(page), Some(request), Some(response)) = (page(), request, response) else {
                return 0;
            };
            if request.resource_type() != ResourceType::MAIN_FRAME {
                return 0;
            }
            let status = u16::try_from(response.status()).unwrap_or(0);
            let retry_after = CefString::from(&response.header_by_name(Some(&"Retry-After".into())))
                .to_string()
                .trim()
                .parse::<u32>()
                .ok();
            let _ = page.with_cycle(|c| c.response(page.now(), status, retry_after));
            0
        }
    }
}

wrap_load_handler! {
    struct Loads;
    impl LoadHandler {
        fn on_load_start(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            _transition_type: TransitionType,
        ) {
            let (Some(page), Some(browser), Some(frame)) = (page(), browser, frame) else {
                return;
            };
            if frame.is_main() == 0 {
                return;
            }
            let _ = page.with_cycle(|c| c.committed(page.now()));
            // Every commit, so a zoom Chromium kept for the host never shows
            // (ADR-0149 decision 4).
            if let Some(host) = browser.host() {
                host.set_zoom_level(zoom_level(page.zoom()));
            }
            let css = page.css();
            if !css.is_empty() {
                let script = css_script(&css);
                frame.execute_java_script(Some(&script.as_str().into()), None, 0);
            }
        }

        fn on_load_end(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            _http_status_code: ::std::os::raw::c_int,
        ) {
            let (Some(page), Some(frame)) = (page(), frame) else {
                return;
            };
            if frame.is_main() == 0 {
                return;
            }
            // Again at the end of the load: Chromium can put its own zoom back
            // while the document commits.
            if let Some(host) = browser.and_then(|b| b.host()) {
                let level = zoom_level(page.zoom());
                if (host.zoom_level() - level).abs() > 1e-6 {
                    host.set_zoom_level(level);
                }
            }
            if page.with_cycle(|c| c.finished(page.now())).unwrap_or(false) {
                page.run_script(frame, &page.script());
            }
        }

        fn on_load_error(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            error_code: Errorcode,
            _error_text: Option<&CefString>,
            _failed_url: Option<&CefString>,
        ) {
            let (Some(page), Some(frame)) = (page(), frame) else {
                return;
            };
            // A stopped or replaced navigation is not a failure of the page.
            if frame.is_main() == 0 || error_code == Errorcode::ABORTED {
                return;
            }
            let _ = page.with_cycle(|c| c.failed(page.now(), None));
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

        fn on_before_browse(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            _request: Option<&mut Request>,
            _user_gesture: ::std::os::raw::c_int,
            is_redirect: ::std::os::raw::c_int,
        ) -> ::std::os::raw::c_int {
            // A main-frame navigation the page started itself: hold its document
            // like a first load. The runner's own reloads told the cycle already.
            if let (Some(page), Some(frame)) = (page(), frame)
                && frame.is_main() != 0
                && is_redirect == 0
                && !page.ours.swap(false, Ordering::AcqRel)
            {
                let _ = page.with_cycle(|c| c.navigate(page.now(), Start::Load));
            }
            0
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
        loads: LoadHandler,
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
        fn load_handler(&self) -> Option<LoadHandler> {
            Some(self.loads.clone())
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
    use objc2_app_kit::{NSApp, NSApplication, NSApplicationActivationPolicy, NSEvent};
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

    /// Makes the shared application our subclass, with no Dock icon, menu bar or
    /// activation: the runner draws off screen only. False when that failed.
    pub fn setup() -> bool {
        let _ = RunnerApplication::shared_application();
        MainThreadMarker::new().is_some_and(|mtm| {
            let app = NSApp(mtm);
            // Before the run loop starts, so the Dock never shows the runner.
            let _ = app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
            app.isKindOfClass(RunnerApplication::class())
        })
    }
}

fn main() {
    // Every process type loads and checks the downloaded engine before any CEF call.
    let engine = match engine::load(false) {
        Ok(engine) => engine,
        Err(code) => std::process::exit(code),
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
    let code = run_browser(&args, &mut app, &engine);
    drop(engine);
    std::process::exit(code);
}

fn run_browser(args: &args::Args, app: &mut App, engine: &engine::Engine) -> i32 {
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
        url: config.url.clone(),
        options: Mutex::new(config.page.clone()),
        css: Mutex::new(page_css(&config.page.css, config.transparent)),
        focused: AtomicBool::new(false),
        cycle: Mutex::new(Cycle::new(CycleOptions::of(
            &config.page,
            config.transparent,
        ))),
        ours: AtomicBool::new(false),
        epoch: Instant::now(),
        fps: i32::try_from(config.fps.clamp(1, 60)).unwrap_or(30),
    });

    // The studio's textures and fence open before CEF starts; when they do not, this
    // process sees another adapter and the tree falls back at once (ADR-0157
    // decision 8). A LUID of 0 is not an adapter, so it falls back too.
    #[cfg(windows)]
    if let Some(share) = config.gpu {
        match gpu::Copier::open(&share) {
            Some(copier) if share.adapter_luid != 0 => {
                GPU_LUID.store(share.adapter_luid, Ordering::Relaxed);
                let _ = COPIER.set(copier);
            }
            _ => {
                // The studio's textures and fence do not open on this adapter.
                eprintln!("rbs-browser-cef: GPU path falls back to shared memory (adapter)");
                return exit::GPU_FALLBACK;
            }
        }
    }

    // Control messages, then stdin closing: the studio's stop (ADR-0109 decision 8,
    // ADR-0132 decision 8). The loop takes them on CEF's UI thread.
    let stop = std::sync::Arc::new(AtomicBool::new(false));
    let stop_flag = std::sync::Arc::clone(&stop);
    let (control_tx, control_rx) = std::sync::mpsc::channel::<Control>();
    let _ = std::thread::Builder::new()
        .name("rbs-browser-stdin".into())
        .spawn(move || {
            // An unknown kind is skipped; a closed pipe or a bad length stops.
            while let Ok(message) = rbs_browser_ipc::control::read(&mut stdin) {
                if let Some(message) = message
                    && control_tx.send(message).is_err()
                {
                    break;
                }
            }
            stop_flag.store(true, Ordering::Release);
        });

    // A signed-in source keeps its profile (ADR-0132 decision 10); every other tree
    // gets a temporary one, removed on exit (ADR-0109 decision 10).
    let kept = !config.page.profile_dir.is_empty();
    let profile = if kept {
        let dir = std::path::PathBuf::from(&config.page.profile_dir);
        if create_private_dir_all(&dir).is_err() {
            return exit::INIT_FAILED;
        }
        dir
    } else {
        let Some(dir) = new_profile_dir() else {
            return exit::INIT_FAILED;
        };
        dir
    };
    let settings = Settings {
        windowless_rendering_enabled: 1,
        // The sandbox is a later step (ADR-0132); until then this runner is opt-in.
        no_sandbox: 1,
        root_cache_path: profile.to_string_lossy().as_ref().into(),
        persist_session_cookies: i32::from(kept),
        log_severity: LogSeverity::ERROR,
        log_file: profile.join("cef.log").to_string_lossy().as_ref().into(),
        resources_dir_path: engine
            .resources_dir()
            .map(|dir| dir.to_string_lossy().as_ref().into())
            .unwrap_or_default(),
        locales_dir_path: engine
            .locales_dir()
            .map(|dir| dir.to_string_lossy().as_ref().into())
            .unwrap_or_default(),
        #[cfg(target_os = "macos")]
        framework_dir_path: engine
            .framework_dir()
            .map(|dir| dir.to_string_lossy().as_ref().into())
            .unwrap_or_default(),
        ..Default::default()
    };
    if initialize(
        Some(args.as_main_args()),
        Some(&settings),
        Some(app),
        std::ptr::null_mut(),
    ) != 1
    {
        if !kept {
            remove_profile(&profile);
        }
        return exit::INIT_FAILED;
    }

    let code = browse(page, &config, &stop, &control_rx);
    shutdown();
    if !kept {
        remove_profile(&profile);
    }
    code
}

/// Creates `dir` and its parents; on Unix the new directories are private to the user.
fn create_private_dir_all(dir: &std::path::Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir)
}

/// A cheap stamp of a directory's entries (names, sizes, modification times), not
/// recursive; it changes when any file in it changes (ADR-0132 decision 5).
fn dir_stamp(dir: &std::path::Path) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        let mut seen: Vec<(std::ffi::OsString, u64, Option<std::time::SystemTime>)> = entries
            .flatten()
            .map(|entry| {
                let meta = entry.metadata().ok();
                (
                    entry.file_name(),
                    meta.as_ref().map_or(0, std::fs::Metadata::len),
                    meta.and_then(|m| m.modified().ok()),
                )
            })
            .collect();
        seen.sort();
        seen.hash(&mut hasher);
    }
    hasher.finish()
}

/// How often a `file` page's directory is checked for `reload_on_file_change`.
const FILE_CHECK: Duration = Duration::from_millis(500);

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
                #[allow(clippy::disallowed_methods)] // After the browser closed, at exit.
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

fn browse(
    page: &Page,
    config: &Configure,
    stop: &AtomicBool,
    control: &std::sync::mpsc::Receiver<Control>,
) -> i32 {
    // The first navigation: the cycle starts in it already.
    page.ours.store(true, Ordering::Release);
    #[cfg_attr(not(windows), allow(unused_mut))] // Set below on Windows only.
    let mut window = WindowInfo::default().set_as_windowless(Default::default());
    #[cfg(windows)]
    if COPIER.get().is_some() {
        window.shared_texture_enabled = 1;
    }
    let browser_settings = BrowserSettings {
        windowless_frame_rate: page.fps,
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
        Requests::new(MainResponses::new()),
        Loads::new(),
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
    // Only a `file` page with the switch on watches its directory; no timer otherwise.
    let watched = config
        .page
        .reload_on_file_change
        .then(|| file_dir(&config.url))
        .flatten();
    let mut stamp = watched.as_deref().map(dir_stamp);
    let mut last_check = Instant::now();

    // Pump CEF at least twice per page frame, and often enough for the heartbeat.
    let period = Duration::from_secs(1) / config.fps.clamp(1, 60);
    let pump = (period / 2).min(Duration::from_millis(10));
    let mut last_beat = Instant::now();
    page.ring.beat();
    #[cfg(windows)]
    let mut repaint_at: Option<Instant> = None;
    #[cfg(windows)]
    let mut shortage = SlotShortage::default();
    while !page.quit.load(Ordering::Acquire) && !stop.load(Ordering::Acquire) {
        do_message_loop_work();
        // A dropped GPU frame is asked for again once, one page frame later, and not
        // again until a paint arrives (ADR-0157 decision 6).
        #[cfg(windows)]
        if GPU_REPAINT.swap(false, Ordering::Relaxed) && repaint_at.is_none() {
            repaint_at = Some(Instant::now() + period);
        }
        // A long shortage of free slots is logged here, never in the callback.
        #[cfg(windows)]
        if let Some(line) =
            shortage.check(NO_SLOT_SINCE.load(Ordering::Relaxed), os_monotonic_nanos())
        {
            eprintln!("rbs-browser-cef: {line}");
        }
        #[cfg(windows)]
        if repaint_at.is_some_and(|at| Instant::now() >= at) {
            repaint_at = None;
            if let Some(host) = browser.host() {
                host.invalidate(PaintElementType::VIEW);
            }
        }
        if last_beat.elapsed() >= HEARTBEAT_PERIOD / 2 && !page.unresponsive.load(Ordering::Acquire)
        {
            page.ring.beat();
            last_beat = Instant::now();
        }
        while let Ok(message) = control.try_recv() {
            apply_control(page, &browser, &message);
        }
        match page.with_cycle(|c| c.tick(page.now())).flatten() {
            Some(Due::Reload) => reload(page, &browser, Start::Reload, false),
            Some(Due::Retry) => reload(page, &browser, Start::Retry, false),
            Some(Due::RunScript) => {
                if let Some(frame) = browser.main_frame() {
                    page.run_script(&frame, &page.script());
                }
            }
            None => {}
        }
        if let Some(dir) = watched.as_deref()
            && last_check.elapsed() >= FILE_CHECK
        {
            last_check = Instant::now();
            let now = dir_stamp(dir);
            if stamp.replace(now) != Some(now) {
                reload(page, &browser, Start::Reload, true);
            }
        }
        #[allow(clippy::disallowed_methods)] // The pump, between callbacks.
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
            #[allow(clippy::disallowed_methods)] // The closing pump, between callbacks.
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    drop(browser);
    page.code.load(Ordering::Acquire)
}

/// Reloads the main frame as `start`, telling the cycle first so the old picture stays
/// until the new document may be sent.
fn reload(page: &Page, browser: &Browser, start: Start, bypass_cache: bool) {
    let _ = page.with_cycle(|c| c.navigate(page.now(), start));
    page.ours.store(true, Ordering::Release);
    if bypass_cache {
        browser.reload_ignore_cache();
    } else {
        browser.reload();
    }
}

/// A control message from the studio, on CEF's UI thread.
fn apply_control(page: &Page, browser: &Browser, message: &Control) {
    match message {
        Control::Reload { bypass_cache } => reload(page, browser, Start::Reload, *bypass_cache),
        Control::RunScript(script) => {
            if let Some(frame) = browser.main_frame() {
                let text = script.clone().unwrap_or_else(|| page.script());
                page.run_script(&frame, &text);
            }
        }
        Control::Page(options) => apply_options(page, browser, options),
        Control::Input(event) => {
            if let Some(host) = browser.host() {
                if !page.focused.swap(true, Ordering::AcqRel) {
                    host.set_focus(1);
                }
                input::send(&host, event);
            }
        }
        // Hidden: one paint per second, and the page is not told it is hidden, so its
        // timers and sockets keep running (ADR-0149 decision 5).
        Control::Shown(shown) => {
            if let Some(host) = browser.host() {
                host.set_windowless_frame_rate(if *shown { page.fps } else { 1 });
            }
        }
    }
}

/// New page options while the page runs (ADR-0149 decision 1): zoom and CSS at once,
/// timers from now, the script from its next run, `retry_on_error` from the next
/// load. A watched directory keeps the setting the page started with.
fn apply_options(page: &Page, browser: &Browser, options: &PageOptions) {
    let transparent = page.transparent;
    let css = page_css(&options.css, transparent);
    let css_changed = page.css.lock().is_ok_and(|mut c| {
        let changed = *c != css;
        *c = css.clone();
        changed
    });
    let zoom_changed = page.options.lock().is_ok_and(|mut o| {
        let changed = o.zoom_percent != options.zoom_percent;
        let profile_dir = std::mem::take(&mut o.profile_dir);
        *o = PageOptions {
            profile_dir,
            ..options.clone()
        };
        changed
    });
    let _ = page.with_cycle(|c| c.set_options(page.now(), CycleOptions::of(options, transparent)));
    if zoom_changed && let Some(host) = browser.host() {
        host.set_zoom_level(zoom_level(options.zoom_percent));
    }
    // The fixed CSS script replaces the sheet it added (an empty one clears it).
    if css_changed && let Some(frame) = browser.main_frame() {
        let script = css_script(&css);
        frame.execute_java_script(Some(&script.as_str().into()), None, 0);
    }
}

#[cfg(test)]
mod slot_shortage_tests {
    use super::{SLOT_SHORTAGE_LOG, SlotShortage};

    const MS: u64 = 1_000_000;

    /// A shortage is logged once after a second and once when it ends; a short one
    /// is not logged (ADR-0157 decision 5).
    #[test]
    fn only_a_long_shortage_is_logged_once_and_its_end() {
        let mut s = SlotShortage::default();
        assert_eq!(s.check(0, 10 * MS), None, "slots free");
        assert_eq!(s.check(100 * MS, 150 * MS), None, "short");
        assert_eq!(s.check(0, 160 * MS), None, "a short one ends quietly");
        let start = 200 * MS;
        assert_eq!(s.check(start, start + SLOT_SHORTAGE_LOG - 1), None);
        assert!(
            s.check(start, start + SLOT_SHORTAGE_LOG).is_some(),
            "after 1 s"
        );
        assert_eq!(s.check(start, start + 3 * SLOT_SHORTAGE_LOG), None, "once");
        let end = s.check(0, start + 1_500 * MS).unwrap();
        assert!(end.contains("1500 ms"), "{end}");
        assert_eq!(s.check(0, start + 2_000 * MS), None);
    }
}

#[cfg(test)]
mod adapter_switch_tests {
    use super::{adapter_luid_switch, gpu_switches};

    #[test]
    fn a_child_process_never_gets_a_gpu_switch() {
        for (luid, pin) in [(0, true), (0, false), (105_241, true), (105_241, false)] {
            assert!(gpu_switches(false, luid, pin).is_empty(), "{luid} {pin}");
        }
    }

    #[test]
    fn the_browser_process_paints_in_software_without_studio_textures() {
        let names: Vec<_> = gpu_switches(true, 0, true)
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(names, ["disable-gpu", "disable-gpu-compositing"]);
    }

    #[test]
    fn the_gpu_path_pins_the_adapter_unless_told_not_to() {
        assert_eq!(
            gpu_switches(true, 105_241, true),
            [("use-adapter-luid", Some("0,105241".to_owned()))]
        );
        assert!(gpu_switches(true, 105_241, false).is_empty());
    }

    #[test]
    fn the_adapter_luid_is_decimal_high_comma_low() {
        assert_eq!(adapter_luid_switch(0x0000_0000_0001_2345), "0,74565");
        assert_eq!(adapter_luid_switch((7 << 32) | 0xffff_fff0), "7,4294967280");
        // A high part with its top bit set is negative, as Chromium's int32 reads it.
        assert_eq!(adapter_luid_switch(0xffff_ffff_0000_0001), "-1,1");
    }
}
