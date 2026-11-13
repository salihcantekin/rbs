//! The macOS backend: target enumeration, permission and the ScreenCaptureKit session.
//!
//! Threads: `open`, `start`, `stop` and `enumerate` run on a control thread;
//! `latest_frame` on the render thread; the sample handler and the stream delegate on
//! ScreenCaptureKit's queues. The render thread only touches the lock-free ring. The
//! producer state is behind a mutex that only the sample handler and `stop` take.

use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::{AnyClass, AnyObject, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{AllocAnyThread, DefinedClass, define_class, msg_send};
use objc2_core_foundation::{
    CFArray, CFDictionary, CFRetained, CFString, CFType, CGPoint, CGRect, CGSize,
};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGColorSpace, CGContext, CGDisplayCopyDisplayMode, CGDisplayMode,
    CGEvent, CGImageAlphaInfo, CGInterpolationQuality, CGMainDisplayID,
    CGPreflightScreenCaptureAccess, CGRectMakeWithDictionaryRepresentation,
    CGRequestScreenCaptureAccess, CGWindowListCopyWindowInfo, CGWindowListOption,
    kCGColorSpaceSRGB, kCGWindowAlpha, kCGWindowBounds, kCGWindowLayer,
};
use objc2_core_media::{CMSampleBuffer, CMTime, CMTimeFlags};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetHeight, CVPixelBufferGetIOSurface,
    CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth,
};
use objc2_foundation::{
    NSArray, NSDictionary, NSError, NSNumber, NSOperatingSystemVersion, NSProcessInfo, NSString,
};
use objc2_io_surface::IOSurfaceRef;
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCFrameStatus, SCScreenshotManager, SCShareableContent, SCStream,
    SCStreamConfiguration, SCStreamDelegate, SCStreamErrorCode, SCStreamFrameInfoContentRect,
    SCStreamFrameInfoContentScale, SCStreamFrameInfoScaleFactor, SCStreamFrameInfoStatus,
    SCStreamOutput, SCStreamOutputType, SCWindow,
};

use rbs_capture::handoff::{FrameMeta, Grave, Graveyard, Ring};
use rbs_capture::{
    CaptureChange, CaptureError, CaptureFactory, CaptureRequest, CaptureSession, CaptureStats,
    CaptureTarget, DisplayReading, DisplayScale, ErrorDomain, GpuDeviceId, Permission,
    REGION_GRAB_LONG, RegionGrab, SyncKind, THUMBNAIL_MAX, TargetId, TargetKind, Thumbnail, caps,
    choose_framebuffer,
};
use rbs_core::capability::{CapabilitySet, CapabilityValue};
use rbs_core::media::{ColorSpace, LeaseOwner, MemoryKind, PixelFormat, Size, VideoFormat};

use rbs_core::registry::{AdapterFactory, AdapterId, HARDWARE, ProbeError};
use rbs_core::time::{Clock, Timestamp};
use rbs_core::timing::{DriftEstimator, FrameRate};

use crate::cursor::{self, Above};
use crate::{ADAPTER_ID, Consumer, FRAMES_IN_FLIGHT, QUEUE_DEPTH, RING_SLOTS};

/// Highest frame rate requested from ScreenCaptureKit (ProMotion displays).
const MAX_RATE: i64 = 120;

/// How long a handed-out surface stays retained after its last use (capture-port.md,
/// native surface rules: at least 10 s).
const HANDLE_GRACE: Duration = Duration::from_secs(10);

/// How often the graveyard's janitor checks stopped sessions.
const JANITOR_PERIOD: Duration = Duration::from_secs(1);

/// Replaced surfaces kept by a running producer before older ones go to the graveyard.
/// ScreenCaptureKit cycles through at most `QUEUE_DEPTH` surfaces per size, so this
/// only fills up after many resizes.
const SEEN_MAX: usize = 64;

/// Longest wait for ScreenCaptureKit's completion handlers on the control thread. The
/// replay daemon answers within a few hundred milliseconds once warm; the first call
/// of a process can take longer (capture-port.md, adapters).
const COMPLETION_TIMEOUT: Duration = Duration::from_secs(2);

/// 'BGRA', `kCVPixelFormatType_32BGRA`.
const PIXEL_FORMAT_BGRA: u32 = 0x4247_5241;

const DISPLAY_PREFIX: &str = "display:";
const WINDOW_PREFIX: &str = "window:";

/// `SCWindow.windowLayer` of normal application windows.
const NORMAL_WINDOW_LAYER: isize = 0;

static GRAVEYARD: Graveyard = Graveyard::new("rbs-sck-janitor", HANDLE_GRACE, JANITOR_PERIOD);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Runs `body` where ScreenCaptureKit called back in. A panic stays in this process.
fn catch_callback(body: impl FnOnce()) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body));
}

/// Test-only: the next completion block panics once, inside [`catch_callback`].
#[cfg(test)]
static COMPLETION_PANIC: AtomicBool = AtomicBool::new(false);

/// Moves an Objective-C or CoreFoundation object to another thread.
///
/// The wrapped objects are immutable data objects (shareable content, windows,
/// displays) or objects Apple documents as usable from any thread (`SCStream`,
/// `CVPixelBuffer`, `IOSurface`); the adapter never mutates them concurrently.
struct Sendable<T>(T);

// SAFETY: see the type's doc: every wrapped type is thread-safe for the way the adapter
// uses it (retain/release are atomic; reads of immutable properties; `SCStream`
// methods are documented as callable from any thread).
unsafe impl<T> Send for Sendable<T> {}

fn ns_error(error: &NSError) -> CaptureError {
    let code = error.code();
    if code == SCStreamErrorCode::UserDeclined.0 {
        return CaptureError::PermissionDenied;
    }
    CaptureError::Backend {
        domain: ErrorDomain::OsStatus,
        code: code as i64,
        recoverable: false,
    }
}

fn timed_out() -> CaptureError {
    CaptureError::Backend {
        domain: ErrorDomain::Library,
        code: 1,
        recoverable: true,
    }
}

/// Completion block for [`wait_completion`]. A panic sends nothing (the wait times out).
fn completion_block(
    tx: mpsc::SyncSender<Result<(), CaptureError>>,
) -> RcBlock<dyn Fn(*mut NSError)> {
    RcBlock::new(move |error: *mut NSError| {
        catch_callback(|| {
            #[cfg(test)]
            if COMPLETION_PANIC.swap(false, Ordering::Relaxed) {
                panic!("completion");
            }
            // SAFETY: ScreenCaptureKit passes null or a valid `NSError` for the call.
            let result = match unsafe { error.as_ref() } {
                Some(error) => Err(ns_error(error)),
                None => Ok(()),
            };
            let _ = tx.try_send(result);
        });
    })
}

/// Runs an API with a completion handler and waits for the handler on this thread.
fn wait_completion(
    call: impl FnOnce(&block2::DynBlock<dyn Fn(*mut NSError)>),
) -> Result<(), CaptureError> {
    let (tx, rx) = mpsc::sync_channel::<Result<(), CaptureError>>(1);
    let block = completion_block(tx);
    call(&block);
    rx.recv_timeout(COMPLETION_TIMEOUT)
        .unwrap_or_else(|_| Err(timed_out()))
}

/// The displays and windows the user may capture. Blocks until ScreenCaptureKit
/// answers (control thread only).
fn shareable_content() -> Result<Retained<SCShareableContent>, CaptureError> {
    shareable_content_within(COMPLETION_TIMEOUT)
}

/// Like [`shareable_content`] with a caller-chosen wait.
fn shareable_content_within(
    timeout: Duration,
) -> Result<Retained<SCShareableContent>, CaptureError> {
    type Answer = Result<Sendable<Retained<SCShareableContent>>, CaptureError>;
    let (tx, rx) = mpsc::sync_channel::<Answer>(1);
    let block = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            catch_callback(|| {
                // SAFETY: ScreenCaptureKit passes null or valid objects for the call; the
                // content is retained before the handler returns.
                let result = match (unsafe { Retained::retain(content) }, unsafe {
                    error.as_ref()
                }) {
                    (Some(content), _) => Ok(Sendable(content)),
                    (None, Some(error)) => Err(ns_error(error)),
                    (None, None) => Err(timed_out()),
                };
                let _ = tx.try_send(result);
            });
        },
    );
    // SAFETY: the block matches the documented handler signature and is retained by
    // ScreenCaptureKit for as long as it needs it.
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&block) };
    match rx.recv_timeout(timeout) {
        Ok(Ok(content)) => Ok(content.0),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(timed_out()),
    }
}

fn display_pixel_size(display: u32) -> (u32, u32) {
    let mode: Option<CFRetained<CGDisplayMode>> = CGDisplayCopyDisplayMode(display);
    let w = CGDisplayMode::pixel_width(mode.as_deref());
    let h = CGDisplayMode::pixel_height(mode.as_deref());
    (clamp_u32(w), clamp_u32(h))
}

fn clamp_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// Pixels per point of a display.
fn display_scale(display: &SCDisplay) -> f64 {
    // SAFETY: plain property reads of an immutable SCDisplay.
    let (id, points) = unsafe { (display.displayID(), display.width()) };
    let (pixels, _) = display_pixel_size(id);
    if points <= 0 || pixels == 0 {
        1.0
    } else {
        f64::from(pixels) / points as f64
    }
}

/// The same ratio as [`display_scale`], as integers, for the Video settings label.
/// The pixel size stays the framebuffer; this ratio is not applied to it.
fn display_scale_ratio(display: &SCDisplay) -> Option<DisplayScale> {
    // SAFETY: plain property reads of an immutable SCDisplay.
    let (id, points) = unsafe { (display.displayID(), display.width()) };
    let (pixels, _) = display_pixel_size(id);
    let points = u32::try_from(points).ok().filter(|p| *p > 0)?;
    DisplayScale::from_pixels_per_point(pixels, points)
}

/// Pixels per point of the display that shows the centre of `window` (the main
/// display's if none does).
fn window_scale(content: &SCShareableContent, window: &SCWindow) -> f64 {
    // SAFETY: plain property reads of immutable shareable-content objects.
    unsafe {
        let frame = window.frame();
        let (cx, cy) = (
            frame.origin.x + frame.size.width / 2.0,
            frame.origin.y + frame.size.height / 2.0,
        );
        let displays = content.displays();
        let contains = |d: &SCDisplay| {
            let r = d.frame();
            cx >= r.origin.x
                && cx < r.origin.x + r.size.width
                && cy >= r.origin.y
                && cy < r.origin.y + r.size.height
        };
        let main = CGMainDisplayID();
        displays
            .iter()
            .find(|d| contains(d))
            .or_else(|| displays.iter().find(|d| d.displayID() == main))
            .map_or(1.0, |d| display_scale(&d))
    }
}

fn rect_pixels(rect: CGRect, scale: f64) -> (u32, u32) {
    let px = |v: f64| (v * scale).round().clamp(0.0, f64::from(u32::MAX)) as u32;
    (px(rect.size.width), px(rect.size.height))
}

fn is_listed_window(window: &SCWindow) -> bool {
    // SAFETY: plain property reads of an immutable SCWindow.
    unsafe {
        window.isOnScreen()
            && window.windowLayer() == NORMAL_WINDOW_LAYER
            && window.title().is_some_and(|t| t.length() > 0)
    }
}

enum Parsed {
    Display(u32),
    Window(u32),
}

fn parse(id: &TargetId) -> Option<Parsed> {
    let id = id.as_str();
    if let Some(rest) = id.strip_prefix(DISPLAY_PREFIX) {
        return rest.parse().ok().map(Parsed::Display);
    }
    id.strip_prefix(WINDOW_PREFIX)?
        .parse()
        .ok()
        .map(Parsed::Window)
}

/// A target found in the shareable content.
enum Resolved {
    Display(Retained<SCDisplay>),
    Window(Retained<SCWindow>),
}

fn resolve(content: &SCShareableContent, parsed: &Parsed) -> Option<(Resolved, (u32, u32))> {
    // SAFETY: plain property reads of immutable shareable-content objects.
    unsafe {
        match *parsed {
            Parsed::Display(id) => {
                let display = content.displays().iter().find(|d| d.displayID() == id)?;
                Some((Resolved::Display(display), display_pixel_size(id)))
            }
            Parsed::Window(id) => {
                let window = content.windows().iter().find(|w| w.windowID() == id)?;
                let size = rect_pixels(window.frame(), window_scale(content, &window));
                Some((Resolved::Window(window), size))
            }
        }
    }
}

fn permission_granted() -> bool {
    CGPreflightScreenCaptureAccess()
}

/// How long a thumbnail grab may take in all (capture-port.md, `thumbnail`).
const THUMBNAIL_TIMEOUT: Duration = Duration::from_millis(1000);

/// `SCScreenshotManager` appeared in macOS 14; the class is looked up by name so an
/// older system gets `Unsupported` instead of a missing-class panic.
fn screenshots_available() -> bool {
    AnyClass::get(c"SCScreenshotManager").is_some()
}

/// A failure of the grab that is not the caller's fault: the picture may work later.
fn grab_failed() -> CaptureError {
    CaptureError::Backend {
        domain: ErrorDomain::Library,
        code: 2,
        recoverable: true,
    }
}

/// The size a `width` x `height` picture is scaled to so it fits in `max`, keeping the
/// aspect, never upscaling, at least 1 x 1. `None` if any input is zero.
fn fit_size(width: u32, height: u32, max: Size) -> Option<(u32, u32)> {
    if width == 0 || height == 0 || max.width == 0 || max.height == 0 {
        return None;
    }
    let scale = (f64::from(max.width) / f64::from(width))
        .min(f64::from(max.height) / f64::from(height))
        .min(1.0);
    let scaled = |v: u32, cap: u32| ((f64::from(v) * scale).round() as u32).clamp(1, cap);
    Some((scaled(width, max.width), scaled(height, max.height)))
}

/// Converts premultiplied RGBA8 to straight alpha in place.
fn unpremultiply(rgba: &mut [u8]) {
    for px in rgba.chunks_exact_mut(4) {
        let a = u32::from(px[3]);
        if a == 0 {
            px[..3].fill(0);
        } else if a < 255 {
            for c in &mut px[..3] {
                *c = ((u32::from(*c) * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
}

/// Draws `image` scaled into a new `width` x `height` sRGB RGBA8 bitmap, straight alpha.
fn image_to_rgba(
    image: &objc2_core_graphics::CGImage,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, CaptureError> {
    let (w, h) = (width as usize, height as usize);
    // SAFETY: `kCGColorSpaceSRGB` is an immutable framework constant.
    let space =
        CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB })).ok_or_else(grab_failed)?;
    let mut rgba = vec![0u8; w * h * 4];
    // SAFETY: `rgba` holds exactly `h` rows of `w * 4` bytes, matching the width,
    // height, bytes-per-row, 8 bits per component and premultiplied-RGBA layout passed
    // here, and it outlives the context, which is dropped before `rgba` is read or moved.
    let context = unsafe {
        CGBitmapContextCreate(
            rgba.as_mut_ptr().cast(),
            w,
            h,
            8,
            w * 4,
            Some(&space),
            CGImageAlphaInfo::PremultipliedLast.0,
        )
    }
    .ok_or_else(grab_failed)?;
    CGContext::set_interpolation_quality(Some(&context), CGInterpolationQuality::High);
    let rect = CGRect::new(
        CGPoint::new(0.0, 0.0),
        CGSize::new(width.into(), height.into()),
    );
    CGContext::draw_image(Some(&context), rect, Some(image));
    drop(context);
    unpremultiply(&mut rgba);
    Ok(rgba)
}

/// One frame of a display or window as a small sRGB picture (ADR-0077 decision 3a).
/// No stream is started and nothing outlives the call; the screenshot API shows no
/// capture border or indicator.
fn grab_thumbnail(target: &TargetId, max: Size) -> Result<Thumbnail, CaptureError> {
    let max = Size::new(
        max.width.min(THUMBNAIL_MAX.width),
        max.height.min(THUMBNAIL_MAX.height),
    );
    let (width, height, rgba) = grab_rgba(target, max, "thumbnail needs macOS 14")?;
    Thumbnail::new(width, height, rgba).ok_or_else(grab_failed)
}

/// Region-section grab (ADR-0103): the same screenshot as a thumbnail, long side ≤ 960.
fn grab_region(target: &TargetId, max_long: u32) -> Result<RegionGrab, CaptureError> {
    if max_long == 0 {
        return Err(CaptureError::FormatUnsupported);
    }
    let side = max_long.min(REGION_GRAB_LONG);
    let (width, height, rgba) =
        grab_rgba(target, Size::new(side, side), "region_grab needs macOS 14")?;
    RegionGrab::new(width, height, rgba).ok_or_else(grab_failed)
}

/// One screenshot scaled to fit in `max`. `unsupported` is the message when the
/// screenshot API is missing (macOS older than 14).
fn grab_rgba(
    target: &TargetId,
    max: Size,
    unsupported: &'static str,
) -> Result<(u32, u32, Vec<u8>), CaptureError> {
    let started = Instant::now();
    let parsed = parse(target).ok_or(CaptureError::TargetNotFound)?;
    if max.width == 0 || max.height == 0 {
        return Err(CaptureError::FormatUnsupported);
    }
    if !screenshots_available() {
        return Err(CaptureError::Unsupported(unsupported));
    }
    if !permission_granted() {
        return Err(CaptureError::PermissionNotDetermined);
    }
    // Backend failures of a grab are recoverable: the next try may find a frame.
    let content = shareable_content_within(THUMBNAIL_TIMEOUT).map_err(|e| match e {
        CaptureError::Backend { .. } => grab_failed(),
        other => other,
    })?;
    let (resolved, native) = resolve(&content, &parsed).ok_or(CaptureError::TargetNotFound)?;
    let (width, height) = fit_size(native.0, native.1, max).ok_or_else(grab_failed)?;
    autoreleasepool(|_| {
        // SAFETY: filters built from objects of the current shareable content; the
        // configuration is fresh and configured before use; the colour space name and
        // pixel format are framework constants. `SCScreenshotManager` (macOS 14) is
        // looked up by class name before this point, so older systems never reach it.
        let (filter, config) = unsafe {
            let filter = match &resolved {
                Resolved::Display(display) => SCContentFilter::initWithDisplay_excludingWindows(
                    SCContentFilter::alloc(),
                    display,
                    &NSArray::new(),
                ),
                Resolved::Window(window) => SCContentFilter::initWithDesktopIndependentWindow(
                    SCContentFilter::alloc(),
                    window,
                ),
            };
            let config = SCStreamConfiguration::new();
            config.setWidth(width as usize);
            config.setHeight(height as usize);
            config.setPixelFormat(PIXEL_FORMAT_BGRA);
            config.setColorSpaceName(kCGColorSpaceSRGB);
            config.setShowsCursor(false);
            (filter, config)
        };
        type Answer = Result<Sendable<CFRetained<objc2_core_graphics::CGImage>>, CaptureError>;
        let (tx, rx) = mpsc::sync_channel::<Answer>(1);
        let block = RcBlock::new(
            move |image: *mut objc2_core_graphics::CGImage, error: *mut NSError| {
                catch_callback(|| {
                    let result = match NonNull::new(image) {
                        // SAFETY: ScreenCaptureKit passes a valid image for the call; it
                        // is retained before the handler returns.
                        Some(image) => Ok(Sendable(unsafe { CFRetained::retain(image) })),
                        // SAFETY: null or a valid `NSError` for the call.
                        None => Err(match unsafe { error.as_ref() } {
                            Some(error) if error.code() == SCStreamErrorCode::UserDeclined.0 => {
                                CaptureError::PermissionDenied
                            }
                            _ => grab_failed(),
                        }),
                    };
                    let _ = tx.try_send(result);
                });
            },
        );
        // SAFETY: the block matches the documented handler signature and is retained
        // by ScreenCaptureKit for as long as it needs it.
        unsafe {
            SCScreenshotManager::captureImageWithFilter_configuration_completionHandler(
                &filter,
                &config,
                Some(&block),
            )
        };
        let left = THUMBNAIL_TIMEOUT.saturating_sub(started.elapsed());
        let image = rx.recv_timeout(left).map_err(|_| grab_failed())??.0;
        let rgba = image_to_rgba(&image, width, height)?;
        Ok((width, height, rgba))
    })
}

/// What the stream is configured with. Shared by the session (reconfigure) and the
/// producer (follows the target's size).
struct Config {
    /// The cursor was asked for.
    cursor: bool,
    /// A window capture's cursor is over a window stacked above the target, so the
    /// stream does not draw it (see `crate::cursor`).
    cursor_covered: bool,
    frame_rate: Option<FrameRate>,
    size: Size,
}

/// Checks the zero-copy part of a request (capture-port.md, zero-copy negotiation).
/// IOSurfaces are global: any Metal device of the machine can import them.
fn accept(request: &CaptureRequest, size: Size) -> Result<Config, CaptureError> {
    if !request.pixel_formats.contains(&PixelFormat::Bgra8)
        || !request.memory.contains(&MemoryKind::Metal)
    {
        return Err(CaptureError::FormatUnsupported);
    }
    let Some(gpu) = request.gpu.as_ref() else {
        return Err(CaptureError::FormatUnsupported);
    };
    if !matches!(gpu.device, GpuDeviceId::MetalRegistryId(_))
        || !gpu.sync.contains(&SyncKind::Ready)
    {
        return Err(CaptureError::FormatUnsupported);
    }
    Ok(Config {
        cursor: request.cursor,
        cursor_covered: false,
        frame_rate: request.frame_rate,
        size,
    })
}

/// Factory of the ScreenCaptureKit adapter.
#[derive(Debug, Default)]
pub struct SckCaptureFactory;

impl SckCaptureFactory {
    /// Creates the factory. Does nothing until used.
    pub fn new() -> Self {
        Self
    }
}

impl AdapterFactory for SckCaptureFactory {
    fn id(&self) -> AdapterId {
        AdapterId::new(ADAPTER_ID)
    }

    fn display_name(&self) -> &str {
        "ScreenCaptureKit"
    }

    fn probe(&self) -> Result<CapabilitySet, ProbeError> {
        // Cheap (capture-port.md: within 50 ms): only the OS version, no
        // ScreenCaptureKit call, which could start its daemon.
        let version = NSOperatingSystemVersion {
            majorVersion: 12,
            minorVersion: 3,
            patchVersion: 0,
        };
        if !NSProcessInfo::processInfo().isOperatingSystemAtLeastVersion(version) {
            return Err(ProbeError::new("ScreenCaptureKit needs macOS 12.3"));
        }
        let set =
            |items: &[&str]| CapabilityValue::Set(items.iter().map(|s| (*s).to_owned()).collect());
        let kinds = ["display", "window"];
        Ok(CapabilitySet::new()
            .with(caps::TARGET_KINDS, set(&kinds))
            .with(caps::TARGET_ENUMERATE, set(&kinds))
            .with(caps::TARGET_PICKER, set(&[]))
            .with(caps::PIXEL_FORMATS, set(&[PixelFormat::Bgra8.as_str()]))
            .with(caps::MEMORY, set(&[MemoryKind::Metal.as_str()]))
            .with(caps::CURSOR, CapabilityValue::Flag(true))
            .with(caps::FRAME_RATE_MAX, CapabilityValue::Int(MAX_RATE))
            .with(
                caps::FRAMES_IN_FLIGHT,
                CapabilityValue::Int(FRAMES_IN_FLIGHT as i64),
            )
            .with(caps::HDR, CapabilityValue::Flag(false))
            .with(HARDWARE, CapabilityValue::Flag(true)))
    }
}

impl CaptureFactory for SckCaptureFactory {
    fn enumerate(&self, kind: TargetKind) -> Result<Vec<CaptureTarget>, CaptureError> {
        if !matches!(kind, TargetKind::Display | TargetKind::Window) {
            return Err(CaptureError::Unsupported(
                "cameras are not captured by ScreenCaptureKit",
            ));
        }
        if !permission_granted() {
            return Err(CaptureError::PermissionNotDetermined);
        }
        let content = shareable_content()?;
        // SAFETY: plain property reads of immutable shareable-content objects.
        unsafe {
            match kind {
                TargetKind::Display => {
                    let main = CGMainDisplayID();
                    Ok(content
                        .displays()
                        .iter()
                        .enumerate()
                        .map(|(index, d)| {
                            let id = d.displayID();
                            let (w, h) = display_pixel_size(id);
                            let scale = display_scale_ratio(&d);
                            // The mode is already the framebuffer. The scale is only the label.
                            let (w, h) = choose_framebuffer(DisplayReading {
                                monitor: None,
                                mode: (w > 0 && h > 0).then_some((w, h)),
                                scale,
                            })
                            .map(|c| (c.width, c.height))
                            .unwrap_or((w, h));
                            let name = if id == main {
                                format!("Display {} (main)", index + 1)
                            } else {
                                format!("Display {}", index + 1)
                            };
                            let target = CaptureTarget::new(
                                TargetId::new(format!("{DISPLAY_PREFIX}{id}")),
                                TargetKind::Display,
                                name,
                            )
                            .with_size(w, h);
                            match scale {
                                Some(scale) => target.with_scale(scale),
                                None => target,
                            }
                        })
                        .collect())
                }
                _ => Ok(content
                    .windows()
                    .iter()
                    .filter(|w| is_listed_window(w))
                    .map(|w| {
                        let (width, height) = rect_pixels(w.frame(), window_scale(&content, &w));
                        let title = w.title().map(|t| t.to_string()).unwrap_or_default();
                        let app = w
                            .owningApplication()
                            .map(|a| a.applicationName().to_string());
                        let name = match &app {
                            Some(app) => format!("{app} - {title}"),
                            None => title,
                        };
                        let target = CaptureTarget::new(
                            TargetId::new(format!("{WINDOW_PREFIX}{}", w.windowID())),
                            TargetKind::Window,
                            name,
                        )
                        .with_size(width, height);
                        match app.filter(|a| !a.is_empty()) {
                            Some(app) => target.with_app(app),
                            None => target,
                        }
                    })
                    .collect()),
            }
        }
    }

    fn open(&self, request: &CaptureRequest) -> Result<Box<dyn CaptureSession>, CaptureError> {
        let parsed = parse(&request.target).ok_or(CaptureError::TargetNotFound)?;
        if !permission_granted() {
            return Err(CaptureError::PermissionNotDetermined);
        }
        // Refuse what cannot be delivered before asking the OS.
        accept(request, Size::new(0, 0))?;
        let content = shareable_content()?;
        let (_, (w, h)) = resolve(&content, &parsed).ok_or(CaptureError::TargetNotFound)?;
        // Screens and windows are captured at their own size (`request.size` is for
        // cameras).
        let size = Size::new(w, h);
        let config = accept(request, size)?;
        Ok(Box::new(SckSession {
            parsed,
            config: Arc::new(Mutex::new(config)),
            clock: Arc::clone(&request.clock),
            format: VideoFormat::new(size, PixelFormat::Bgra8, ColorSpace::SRGB),
            active: None,
            next_sequence: 0,
            last_timestamp: None,
            base: CaptureStats::default(),
        }))
    }

    fn thumbnail(&self, target: &TargetId, max: Size) -> Result<Thumbnail, CaptureError> {
        grab_thumbnail(target, max)
    }

    fn region_grab(&self, target: &TargetId, max_long: u32) -> Result<RegionGrab, CaptureError> {
        grab_region(target, max_long)
    }

    fn permission(&self, kind: TargetKind) -> Permission {
        match kind {
            TargetKind::Display | TargetKind::Window if !permission_granted() => {
                // macOS does not tell "not asked" from "refused"; the request answers.
                Permission::NotDetermined
            }
            _ => Permission::Granted,
        }
    }

    fn request_permission(&self, kind: TargetKind) -> Result<Permission, CaptureError> {
        match kind {
            TargetKind::Display | TargetKind::Window => {
                if permission_granted() || CGRequestScreenCaptureAccess() {
                    Ok(Permission::Granted)
                } else {
                    Ok(Permission::Denied)
                }
            }
            _ => Ok(Permission::Granted),
        }
    }
}

/// A pixel buffer held by a ring slot, with the surface it names.
struct Held {
    buffer: CFRetained<CVPixelBuffer>,
    surface: CFRetained<IOSurfaceRef>,
}

/// The ring and the pixel buffers its slots name. Frames hold an `Arc` of it.
struct Shared {
    ring: Ring,
    /// One entry per slot. Only the producer (sample handler, or `stop`) locks it.
    slots: Mutex<Vec<Option<Sendable<Held>>>>,
}

impl AsRef<Ring> for Shared {
    fn as_ref(&self) -> &Ring {
        &self.ring
    }
}

// SAFETY: delegates to `Ring`, whose counting is exact when its calling protocol is
// followed (`rbs_capture::handoff::Ring`): the producer publishes, abandons and
// replaces only slots it claimed, and every lease releases exactly the reference it
// adopted or retained. A slot's pixel buffer (and so its IOSurface) is retained in
// `slots` and only replaced while the slot is claimed, so it stays valid and unwritten
// by ScreenCaptureKit while any lease (which holds an `Arc<Shared>`) refers to it.
// Neither method locks.
unsafe impl LeaseOwner for Shared {
    fn retain(&self, token: u64) {
        self.ring.retain(token);
    }

    fn release(&self, token: u64) {
        self.ring.release(token);
    }
}

/// Surfaces a frame was handed out with and that the producer no longer holds, kept
/// for the grace so their pointer values are not reused while an importer may cache
/// them.
struct Seen {
    entries: Vec<(Sendable<CFRetained<IOSurfaceRef>>, Instant)>,
}

impl Seen {
    fn with_capacity() -> Self {
        Self {
            entries: Vec::with_capacity(SEEN_MAX),
        }
    }

    /// Keeps `surface` from `now`. Never allocates: when full, the oldest entry goes to
    /// the graveyard, which keeps it for the rest of its grace.
    fn keep(&mut self, surface: CFRetained<IOSurfaceRef>, now: Instant) {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|(s, _)| std::ptr::eq(&*s.0, &*surface))
        {
            entry.1 = now;
            return;
        }
        if self.entries.len() == SEEN_MAX {
            let (oldest, since) = self.entries.remove(0);
            GRAVEYARD.bury(Box::new(SurfaceGrave {
                _surface: oldest,
                until: since + HANDLE_GRACE,
            }));
        }
        self.entries.push((Sendable(surface), now));
    }

    /// Releases surfaces whose grace has passed.
    fn expire(&mut self, now: Instant) {
        self.entries
            .retain(|(_, since)| now.saturating_duration_since(*since) < HANDLE_GRACE);
    }
}

/// One surface moved out of a full [`Seen`] list. Kept until its own grace ends.
struct SurfaceGrave {
    _surface: Sendable<CFRetained<IOSurfaceRef>>,
    until: Instant,
}

impl Grave for SurfaceGrave {
    fn is_idle(&self) -> bool {
        // The graveyard adds its own grace from the first idle sample.
        Instant::now() + HANDLE_GRACE >= self.until
    }
    fn bytes(&self) -> u64 {
        0
    }
}

/// The surfaces of a stopped capture. Kept while frames reference them (so the last
/// lease never frees anything on the render thread) and for the grace after that.
struct RingGrave {
    shared: Arc<Shared>,
    _seen: Seen,
    bytes: u64,
}

impl Grave for RingGrave {
    fn is_idle(&self) -> bool {
        self.shared.ring.is_idle()
    }
    fn bytes(&self) -> u64 {
        self.bytes
    }
}

/// Producer state, touched by the sample handler and by `stop`.
struct Producer {
    shared: Arc<Shared>,
    config: Arc<Mutex<Config>>,
    /// The stream, to follow the target's size. Set once the stream exists.
    stream: Option<Sendable<Retained<SCStream>>>,
    /// The native size seen on the previous complete frame.
    last_native: Option<(u32, u32)>,
    lost: Arc<AtomicBool>,
    seen: Seen,
    sequence: u64,
    clock: Arc<dyn Clock>,
    drift: DriftEstimator,
    last_timestamp: Option<Timestamp>,
    stopped: bool,
}

/// Nanoseconds on the host clock (`mach_absolute_time`), the clock of the sample
/// buffers' presentation times.
fn host_nanos() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid out pointer.
    let _ = unsafe { libc::clock_gettime(libc::CLOCK_UPTIME_RAW, &mut ts) };
    let secs = u64::try_from(ts.tv_sec).unwrap_or(0);
    let nanos = u64::try_from(ts.tv_nsec).unwrap_or(0);
    secs.saturating_mul(1_000_000_000).saturating_add(nanos)
}

fn cm_time_nanos(time: CMTime) -> Option<u64> {
    if !time.flags.contains(CMTimeFlags::Valid) || time.timescale <= 0 || time.value < 0 {
        return None;
    }
    let nanos = i128::from(time.value) * 1_000_000_000 / i128::from(time.timescale);
    u64::try_from(nanos).ok()
}

/// What ScreenCaptureKit attaches to a screen sample.
struct FrameInfo {
    status: SCFrameStatus,
    /// The target's own size in pixels, before ScreenCaptureKit scaled it to fit the
    /// configured output.
    native_size: Option<(u32, u32)>,
}

fn number(dict: &NSDictionary<NSString, AnyObject>, key: &NSString) -> Option<Retained<NSNumber>> {
    dict.objectForKey(key)?.downcast::<NSNumber>().ok()
}

/// Reads the attachments of a sample.
fn frame_info(sample: &CMSampleBuffer) -> Option<FrameInfo> {
    // SAFETY: reads the sample's attachments array without creating it.
    let attachments = unsafe { sample.sample_attachments_array(false) }?;
    if attachments.count() < 1 {
        return None;
    }
    // SAFETY: index 0 exists; the element is a CF object kept alive by `attachments`
    // during this function. Its type is checked before it is used as a dictionary.
    let element = unsafe {
        NonNull::new(attachments.value_at_index(0).cast_mut())?
            .cast::<CFType>()
            .as_ref()
    };
    let cf_dict = element.downcast_ref::<CFDictionary>()?;
    // SAFETY: CFDictionary is toll-free bridged to NSDictionary; the attachment keys
    // are strings.
    let dict =
        unsafe { &*(cf_dict as *const CFDictionary).cast::<NSDictionary<NSString, AnyObject>>() };
    // SAFETY: the keys are immutable framework constants.
    let (status_key, rect_key, content_scale_key, scale_key) = unsafe {
        (
            SCStreamFrameInfoStatus,
            SCStreamFrameInfoContentRect,
            SCStreamFrameInfoContentScale,
            SCStreamFrameInfoScaleFactor,
        )
    };
    let status = SCFrameStatus(number(dict, status_key)?.integerValue());
    let native_size = (|| {
        let rect_dict = dict
            .objectForKey(rect_key)?
            .downcast::<NSDictionary>()
            .ok()?;
        let mut rect = CGRect::default();
        // SAFETY: a dictionary of a CGRect (CGRectCreateDictionaryRepresentation), toll-
        // free bridged; `rect` is a valid out pointer.
        let ok = unsafe {
            let cf = &*(&*rect_dict as *const NSDictionary).cast::<CFDictionary>();
            CGRectMakeWithDictionaryRepresentation(Some(cf), &mut rect)
        };
        let content_scale = number(dict, content_scale_key)?.doubleValue();
        let scale = number(dict, scale_key)?.doubleValue();
        if !ok || content_scale <= 0.0 || scale <= 0.0 {
            return None;
        }
        // The content rect is in points of the output; undo the fit scaling.
        let size = rect_pixels(rect, scale / content_scale);
        (size.0 > 0 && size.1 > 0).then_some(size)
    })();
    Some(FrameInfo {
        status,
        native_size,
    })
}

impl Producer {
    fn on_sample(&mut self, sample: &CMSampleBuffer) {
        if self.stopped {
            return;
        }
        let Some(info) = frame_info(sample) else {
            return;
        };
        if info.status == SCFrameStatus::Stopped {
            // The window closed or the display went away; the delegate is not always
            // told (invariant 7).
            self.lost.store(true, Ordering::Release);
            return;
        }
        if info.status != SCFrameStatus::Complete {
            return;
        }
        if let Some(native) = info.native_size {
            self.follow_size(native);
        }
        let arrived = self.clock.now();
        let host = host_nanos();
        // SAFETY: the image buffer of a complete screen sample is a CVPixelBuffer.
        let Some(buffer) = (unsafe { sample.image_buffer() }) else {
            return;
        };
        if CVPixelBufferGetPixelFormatType(&buffer) != PIXEL_FORMAT_BGRA {
            return;
        }
        let Some(surface) = CVPixelBufferGetIOSurface(Some(&buffer)) else {
            return;
        };
        let width = clamp_u32(CVPixelBufferGetWidth(&buffer));
        let height = clamp_u32(CVPixelBufferGetHeight(&buffer));
        let now = Instant::now();
        self.seen.expire(now);

        let shared = Arc::clone(&self.shared);
        let ring = &shared.ring;
        let Some(slot) = ring.claim() else {
            // The consumer holds every slot: drop rather than wait (invariant 9).
            ring.count_drop();
            self.sequence += 1;
            return;
        };
        {
            let mut slots = lock(&shared.slots);
            let Some(entry) = slots.get_mut(slot) else {
                ring.abandon(slot);
                return;
            };
            let value = CFRetained::as_ptr(&surface).as_ptr() as u64;
            if ring.handle(slot) != Some(value) {
                if let Some(old) = entry.take() {
                    // A surface a frame was handed out with keeps its pointer value
                    // reserved for the grace; others go back to ScreenCaptureKit now.
                    if ring.handed_out(slot) {
                        self.seen.keep(old.0.surface, now);
                    }
                }
                ring.set_handle(slot, value);
            }
            *entry = Some(Sendable(Held { buffer, surface }));
        }

        // Clock edge (ADR-0004): pair the host clock with the master clock now, then
        // convert the sample's host-clock presentation time.
        // SAFETY: reading the timing of a valid sample buffer.
        let source = cm_time_nanos(unsafe { sample.presentation_time_stamp() });
        let mapped = match source {
            Some(source) => {
                let _ = self.drift.observe(host, arrived);
                self.drift
                    .mapping()
                    .map_or(arrived, |m| m.to_master(source))
            }
            None => arrived,
        };
        let floor = self
            .last_timestamp
            .map_or(Timestamp::ZERO, |t| t + Duration::from_nanos(1));
        let timestamp = mapped.min(arrived).max(floor);
        self.last_timestamp = Some(timestamp);

        ring.publish(
            slot,
            FrameMeta::new(
                timestamp.as_nanos(),
                self.sequence,
                source.unwrap_or(0),
                width,
                height,
            ),
        );
        self.sequence += 1;
    }

    /// Reconfigures the stream when the target's size changed, so later frames are
    /// delivered at the new size (invariant 6) instead of scaled into the old one.
    fn follow_size(&mut self, native: (u32, u32)) {
        if self.last_native == Some(native) {
            return;
        }
        self.last_native = Some(native);
        let Some(stream) = self.stream.as_ref() else {
            return;
        };
        let mut config = lock(&self.config);
        let size = Size::new(native.0, native.1);
        if config.size == size {
            return;
        }
        config.size = size;
        // Sent under the lock, so updates from the cursor watcher keep their order.
        send_configuration(&stream.0, &config);
    }
}

struct OutputIvars {
    producer: Arc<Mutex<Producer>>,
    lost: Arc<AtomicBool>,
}

define_class!(
    // SAFETY:
    // - NSObject has no subclassing requirements.
    // - `RbsSckOutput` does not implement `Drop`.
    #[unsafe(super(NSObject))]
    #[name = "RbsSckOutput"]
    #[ivars = OutputIvars]
    struct Output;

    unsafe impl NSObjectProtocol for Output {}

    unsafe impl SCStreamOutput for Output {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn stream_did_output(
            &self,
            _stream: &SCStream,
            sample: &CMSampleBuffer,
            kind: SCStreamOutputType,
        ) {
            // A panic must not unwind into ScreenCaptureKit. The frame is dropped.
            catch_callback(|| {
                if kind == SCStreamOutputType::Screen {
                    // GCD queues do not always drain an autorelease pool per work item.
                    autoreleasepool(|_| lock(&self.ivars().producer).on_sample(sample));
                }
            });
        }
    }

    unsafe impl SCStreamDelegate for Output {
        #[unsafe(method(stream:didStopWithError:))]
        fn stream_did_stop(&self, _stream: &SCStream, _error: &NSError) {
            let lost = &self.ivars().lost;
            // A panic must not unwind into ScreenCaptureKit. The stream still counts
            // as stopped: that is the error status for this callback.
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                lost.store(true, Ordering::Release);
            }))
            .is_err()
            {
                lost.store(true, Ordering::Release);
            }
        }
    }
);

impl Output {
    fn new(ivars: OutputIvars) -> Retained<Self> {
        let this = Self::alloc().set_ivars(ivars);
        // SAFETY: `init` of NSObject on a freshly allocated instance.
        unsafe { msg_send![super(this), init] }
    }
}

struct Active {
    stream: Sendable<Retained<SCStream>>,
    /// Window captures with the cursor: hides it while it is over another window.
    cursor_watch: Option<CursorWatch>,
    output: Sendable<Retained<Output>>,
    _queue: Sendable<DispatchRetained<DispatchQueue>>,
    producer: Arc<Mutex<Producer>>,
    consumer: Consumer<Shared>,
    shared: Arc<Shared>,
    lost: Arc<AtomicBool>,
}

struct SckSession {
    parsed: Parsed,
    config: Arc<Mutex<Config>>,
    clock: Arc<dyn Clock>,
    format: VideoFormat,
    active: Option<Active>,
    next_sequence: u64,
    last_timestamp: Option<Timestamp>,
    /// Counters of earlier Active periods.
    base: CaptureStats,
}

fn frame_interval(rate: Option<FrameRate>) -> CMTime {
    let per = rate.map_or(Duration::ZERO, FrameRate::frame_duration);
    CMTime {
        value: i64::try_from(per.as_nanos()).unwrap_or(i64::MAX),
        timescale: 1_000_000_000,
        flags: CMTimeFlags::Valid,
        epoch: 0,
    }
}

/// Sends `config` to a running stream without waiting for it to apply, so the caller
/// (the sample handler, the cursor watcher) never blocks. Callers hold the config lock
/// so updates reach ScreenCaptureKit in the order they were made.
fn send_configuration(stream: &SCStream, config: &Config) {
    let config = configuration(config);
    // SAFETY: a valid configuration; no completion handler.
    unsafe { stream.updateConfiguration_completionHandler(&config, None) };
}

fn configuration(config: &Config) -> Retained<SCStreamConfiguration> {
    let size = config.size;
    // SAFETY: a fresh configuration object, configured before use; the colour space
    // name is a framework constant.
    unsafe {
        let stream_config = SCStreamConfiguration::new();
        stream_config.setWidth(size.width as usize);
        stream_config.setHeight(size.height as usize);
        stream_config.setPixelFormat(PIXEL_FORMAT_BGRA);
        stream_config.setColorSpaceName(kCGColorSpaceSRGB);
        stream_config.setShowsCursor(config.cursor && !config.cursor_covered);
        stream_config.setQueueDepth(QUEUE_DEPTH as isize);
        stream_config.setMinimumFrameInterval(frame_interval(config.frame_rate));
        stream_config
    }
}

impl SckSession {
    fn activate(&mut self) -> Result<Active, CaptureError> {
        let content = shareable_content()?;
        let (resolved, _) = resolve(&content, &self.parsed).ok_or(CaptureError::TargetNotFound)?;
        // SAFETY: filters built from objects of the current shareable content.
        let filter = unsafe {
            match &resolved {
                Resolved::Display(display) => SCContentFilter::initWithDisplay_excludingWindows(
                    SCContentFilter::alloc(),
                    display,
                    &NSArray::new(),
                ),
                Resolved::Window(window) => SCContentFilter::initWithDesktopIndependentWindow(
                    SCContentFilter::alloc(),
                    window,
                ),
            }
        };
        let window = match self.parsed {
            Parsed::Window(id) => Some(id),
            Parsed::Display(_) => None,
        };
        let config = {
            let mut config = lock(&self.config);
            config.cursor_covered = match window {
                Some(id) if config.cursor => cursor_covered(id),
                _ => false,
            };
            configuration(&config)
        };

        let shared = Arc::new(Shared {
            ring: Ring::new(&[0; RING_SLOTS]),
            slots: Mutex::new((0..RING_SLOTS).map(|_| None).collect()),
        });
        let lost = Arc::new(AtomicBool::new(false));
        let producer = Arc::new(Mutex::new(Producer {
            shared: Arc::clone(&shared),
            config: Arc::clone(&self.config),
            stream: None,
            last_native: None,
            lost: Arc::clone(&lost),
            seen: Seen::with_capacity(),
            sequence: self.next_sequence,
            clock: Arc::clone(&self.clock),
            drift: DriftEstimator::new(Duration::from_millis(100)),
            last_timestamp: self.last_timestamp,
            stopped: false,
        }));
        let output = Output::new(OutputIvars {
            producer: Arc::clone(&producer),
            lost: Arc::clone(&lost),
        });
        let queue = DispatchQueue::new("org.rbs.capture.sck", None);
        let delegate = ProtocolObject::<dyn SCStreamDelegate>::from_ref(&*output);
        // SAFETY: the filter and configuration are valid; the delegate is retained by
        // `Active` for the life of the stream.
        let stream = unsafe {
            SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &config,
                Some(delegate),
            )
        };
        let sink = ProtocolObject::<dyn SCStreamOutput>::from_ref(&*output);
        // SAFETY: the output and queue outlive the stream's use of them (both are kept
        // in `Active` until `stop` removes the output).
        unsafe {
            stream.addStreamOutput_type_sampleHandlerQueue_error(
                sink,
                SCStreamOutputType::Screen,
                Some(&queue),
            )
        }
        .map_err(|e| ns_error(&e))?;
        // SAFETY: the completion block matches the documented signature.
        let started = wait_completion(|block| unsafe {
            stream.startCaptureWithCompletionHandler(Some(block))
        });
        if let Err(e) = started {
            // A late start would otherwise leave capture running with no owner.
            // SAFETY: stopping and removing what was started and added above.
            let _ = unsafe {
                stream.stopCaptureWithCompletionHandler(None);
                stream.removeStreamOutput_type_error(sink, SCStreamOutputType::Screen)
            };
            return Err(e);
        }
        lock(&producer).stream = Some(Sendable(stream.clone()));
        // SAFETY: `shared`'s ring only ever names IOSurfaces of BGRA8 pixel buffers of
        // the published size, retained in `shared.slots` while their slot is referenced
        // (see the `LeaseOwner` impl); ScreenCaptureKit delivers complete frames only.
        let consumer = unsafe { Consumer::new(Arc::clone(&shared), self.format) };
        let cursor_watch = match window {
            Some(id) if lock(&self.config).cursor => {
                CursorWatch::start(id, &stream, Arc::clone(&self.config))
            }
            _ => None,
        };
        Ok(Active {
            stream: Sendable(stream),
            cursor_watch,
            output: Sendable(output),
            _queue: Sendable(queue),
            producer,
            consumer,
            shared,
            lost,
        })
    }

    fn update_configuration(&self) -> Result<(), CaptureError> {
        let Some(active) = self.active.as_ref() else {
            return Ok(());
        };
        let config = configuration(&lock(&self.config));
        let stream = &active.stream.0;
        // SAFETY: a valid configuration and a completion block of the documented
        // signature.
        wait_completion(|block| unsafe {
            stream.updateConfiguration_completionHandler(&config, Some(block));
        })
    }
}

impl CaptureSession for SckSession {
    fn format(&self) -> VideoFormat {
        self.active
            .as_ref()
            .map_or(self.format, |a| a.consumer.format())
    }

    fn start(&mut self) -> Result<(), CaptureError> {
        if self.active.is_some() {
            return Ok(());
        }
        let active = self.activate()?;
        self.active = Some(active);
        Ok(())
    }

    fn stop(&mut self) {
        let Some(mut active) = self.active.take() else {
            return;
        };
        if let Some(watch) = active.cursor_watch.take() {
            watch.stop();
        }
        let stream = &active.stream.0;
        // SAFETY: a completion block of the documented signature.
        let _ = wait_completion(|block| unsafe {
            stream.stopCaptureWithCompletionHandler(Some(block));
        });
        let sink = ProtocolObject::<dyn SCStreamOutput>::from_ref(&*active.output.0);
        // SAFETY: removing the output added in `activate`.
        let _ = unsafe { stream.removeStreamOutput_type_error(sink, SCStreamOutputType::Screen) };
        let seen = {
            // Waits for a handler that is still running, then makes later ones no-ops.
            let mut producer = lock(&active.producer);
            producer.stopped = true;
            producer.stream = None;
            self.next_sequence = producer.sequence;
            self.last_timestamp = producer.last_timestamp;
            std::mem::replace(
                &mut producer.seen,
                Seen {
                    entries: Vec::new(),
                },
            )
        };
        let ring = &active.shared.ring;
        let waiting = ring.clear();
        self.base.delivered += active.consumer.delivered();
        self.base.dropped += ring.dropped() + u64::from(waiting);
        self.format = active.consumer.format();
        // Pixel buffers nobody took a frame of never left the adapter: give them back
        // now. The rest stay in the graveyard while frames reference them and for the
        // grace after that.
        let bytes = {
            let mut slots = lock(&active.shared.slots);
            let mut bytes = 0u64;
            for (slot, entry) in slots.iter_mut().enumerate() {
                if ring.is_free(slot) && !ring.handed_out(slot) {
                    *entry = None;
                } else if let Some(held) = entry.as_ref() {
                    let w = CVPixelBufferGetWidth(&held.0.buffer) as u64;
                    let h = CVPixelBufferGetHeight(&held.0.buffer) as u64;
                    bytes += w * h * 4;
                }
            }
            bytes
        };
        GRAVEYARD.bury(Box::new(RingGrave {
            shared: Arc::clone(&active.shared),
            _seen: seen,
            bytes,
        }));
    }

    fn latest_frame(&mut self) -> Result<Option<rbs_core::media::VideoFrame>, CaptureError> {
        let Some(active) = self.active.as_mut() else {
            return Ok(None);
        };
        if active.lost.load(Ordering::Acquire) {
            return Err(CaptureError::TargetLost);
        }
        Ok(active.consumer.next())
    }

    fn stats(&self) -> CaptureStats {
        let mut stats = self.base;
        if let Some(active) = self.active.as_ref() {
            stats.delivered += active.consumer.delivered();
            stats.dropped += active.shared.ring.dropped();
        }
        stats
    }

    fn reconfigure(&mut self, change: CaptureChange) -> Result<(), CaptureError> {
        match change {
            CaptureChange::Cursor(on) => {
                {
                    let mut config = lock(&self.config);
                    config.cursor = on;
                    config.cursor_covered = false;
                }
                if let (Some(active), Parsed::Window(id)) = (self.active.as_mut(), &self.parsed) {
                    match (on, active.cursor_watch.take()) {
                        (true, None) => {
                            active.cursor_watch =
                                CursorWatch::start(*id, &active.stream.0, Arc::clone(&self.config));
                        }
                        (true, Some(watch)) => active.cursor_watch = Some(watch),
                        (false, Some(watch)) => watch.stop(),
                        (false, None) => {}
                    }
                }
                self.update_configuration()
            }
            CaptureChange::FrameRate(rate) => {
                lock(&self.config).frame_rate = Some(rate);
                self.update_configuration()
            }
            _ => Err(CaptureError::Unsupported("change not supported")),
        }
    }
}

/// The pointer in global display points, origin top left.
fn pointer() -> Option<(f64, f64)> {
    let event = CGEvent::new(None)?;
    let point = CGEvent::location(Some(&event));
    Some((point.x, point.y))
}

/// Reads an `NSString` key of a CoreGraphics constant.
fn ns_key(key: &CFString) -> &NSString {
    // SAFETY: CFString is toll-free bridged to NSString.
    unsafe { &*(key as *const CFString).cast::<NSString>() }
}

/// The on-screen windows stacked above `window`, from the window server's list.
fn windows_above(window: u32) -> Vec<Above> {
    let options =
        CGWindowListOption::OptionOnScreenAboveWindow | CGWindowListOption::ExcludeDesktopElements;
    let Some(list) = CGWindowListCopyWindowInfo(options, window) else {
        return Vec::new();
    };
    // SAFETY: CFArray is toll-free bridged to NSArray; the window list holds
    // dictionaries with string keys.
    let list = unsafe {
        &*(&*list as *const CFArray).cast::<NSArray<NSDictionary<NSString, AnyObject>>>()
    };
    // SAFETY: the keys are immutable framework constants.
    let (bounds_key, alpha_key, layer_key) = unsafe {
        (
            ns_key(kCGWindowBounds),
            ns_key(kCGWindowAlpha),
            ns_key(kCGWindowLayer),
        )
    };
    list.iter()
        .filter_map(|info| {
            let bounds = info
                .objectForKey(bounds_key)?
                .downcast::<NSDictionary>()
                .ok()?;
            let mut rect = CGRect::default();
            // SAFETY: a dictionary of a CGRect (kCGWindowBounds), toll-free bridged;
            // `rect` is a valid out pointer.
            let ok = unsafe {
                let cf = &*(&*bounds as *const NSDictionary).cast::<CFDictionary>();
                CGRectMakeWithDictionaryRepresentation(Some(cf), &mut rect)
            };
            let alpha = number(&info, alpha_key).map_or(1.0, |n| n.doubleValue());
            let layer = number(&info, layer_key).map_or(0, |n| n.integerValue() as i64);
            ok.then_some(Above {
                bounds: cursor::Rect {
                    x: rect.origin.x,
                    y: rect.origin.y,
                    width: rect.size.width,
                    height: rect.size.height,
                },
                alpha,
                layer,
            })
        })
        .collect()
}

/// Whether a window stacked above `window` is under the pointer at `point`.
fn covered_at(window: u32, point: Option<(f64, f64)>) -> bool {
    autoreleasepool(|_| point.is_some_and(|(x, y)| cursor::covered(x, y, windows_above(window))))
}

/// Whether a window stacked above `window` is under the pointer now.
fn cursor_covered(window: u32) -> bool {
    covered_at(window, pointer())
}

/// The watcher thread of an Active window capture with the cursor (`crate::cursor`).
/// Exists only while Active (P13); `stop` joins it.
struct CursorWatch {
    stop: mpsc::Sender<()>,
    thread: JoinHandle<()>,
}

impl CursorWatch {
    /// Starts watching; `None` if the thread cannot start, which leaves the cursor
    /// drawn as ScreenCaptureKit does by default.
    fn start(window: u32, stream: &Retained<SCStream>, config: Arc<Mutex<Config>>) -> Option<Self> {
        let (stop, stopped) = mpsc::channel::<()>();
        let stream = Sendable(stream.clone());
        let thread = std::thread::Builder::new()
            .name("rbs-sck-cursor".into())
            .spawn(move || {
                let stream = stream;
                let mut last_point = None;
                let mut still_polls = 0u32;
                // Ends when `stop` drops the sender (or sends).
                while let Err(mpsc::RecvTimeoutError::Timeout) = stopped.recv_timeout(cursor::POLL)
                {
                    let point = pointer();
                    let moved = point != last_point;
                    last_point = point;
                    if !cursor::should_query(moved, still_polls) {
                        still_polls += 1;
                        continue;
                    }
                    still_polls = 0;
                    let covered = covered_at(window, point);
                    let mut config = lock(&config);
                    if config.cursor && config.cursor_covered != covered {
                        config.cursor_covered = covered;
                        autoreleasepool(|_| send_configuration(&stream.0, &config));
                    }
                }
            })
            .ok()?;
        Some(Self { stop, thread })
    }

    fn stop(self) {
        drop(self.stop);
        let _ = self.thread.join();
    }
}

impl Drop for SckSession {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod thumbnail_tests {
    #![allow(clippy::expect_used)]

    use super::*;

    #[test]
    fn fit_keeps_aspect_and_never_upscales() {
        let max = THUMBNAIL_MAX;
        assert_eq!(fit_size(3840, 2160, max), Some((192, 108)));
        assert_eq!(fit_size(1000, 1000, max), Some((108, 108)));
        assert_eq!(fit_size(100, 50, max), Some((100, 50)));
        assert_eq!(fit_size(10_000, 10, max), Some((192, 1)));
        assert_eq!(fit_size(0, 10, max), None);
        assert_eq!(fit_size(10, 10, Size::new(0, 5)), None);
        assert_eq!(fit_size(3840, 2160, Size::new(960, 960)), Some((960, 540)));
        assert_eq!(fit_size(800, 600, Size::new(960, 960)), Some((800, 600)));
    }

    #[test]
    fn straight_alpha_is_recovered() {
        let mut px = [128, 64, 0, 128, 9, 9, 9, 0, 10, 20, 30, 255];
        unpremultiply(&mut px);
        assert_eq!(px, [255, 128, 0, 128, 0, 0, 0, 0, 10, 20, 30, 255]);
    }

    #[test]
    fn refuses_what_it_cannot_grab() {
        let factory = SckCaptureFactory::new();
        let nothing = Size::new(0, 0);
        assert!(matches!(
            factory.thumbnail(&TargetId::new("nope"), THUMBNAIL_MAX),
            Err(CaptureError::TargetNotFound)
        ));
        assert!(matches!(
            factory.thumbnail(&TargetId::new(format!("{DISPLAY_PREFIX}1")), nothing),
            Err(CaptureError::FormatUnsupported)
        ));
        assert!(matches!(
            factory.region_grab(&TargetId::new("nope"), REGION_GRAB_LONG),
            Err(CaptureError::TargetNotFound)
        ));
        assert!(matches!(
            factory.region_grab(&TargetId::new(format!("{DISPLAY_PREFIX}1")), 0),
            Err(CaptureError::FormatUnsupported)
        ));
    }

    /// With screen recording permission: the main display yields a picture that fits.
    /// Set `RBS_THUMB_DUMP=<dir>` to also write it as a PPM for a look.
    #[test]
    fn main_display_thumbnail() {
        if !permission_granted() || !screenshots_available() {
            eprintln!("skipped: needs screen recording permission and macOS 14");
            return;
        }
        let factory = SckCaptureFactory::new();
        let id = TargetId::new(format!("{DISPLAY_PREFIX}{}", CGMainDisplayID()));
        let thumb = factory.thumbnail(&id, THUMBNAIL_MAX).expect("thumbnail");
        assert!(thumb.width() <= THUMBNAIL_MAX.width && thumb.height() <= THUMBNAIL_MAX.height);
        assert!(thumb.width() == THUMBNAIL_MAX.width || thumb.height() == THUMBNAIL_MAX.height);
        assert!(
            thumb.rgba().chunks_exact(4).any(|p| p[..3] != [0, 0, 0]),
            "all black"
        );
        if let Some(dir) = std::env::var_os("RBS_THUMB_DUMP") {
            let mut ppm = format!("P6\n{} {}\n255\n", thumb.width(), thumb.height()).into_bytes();
            for p in thumb.rgba().chunks_exact(4) {
                ppm.extend_from_slice(&p[..3]);
            }
            std::fs::write(std::path::Path::new(&dir).join("display.ppm"), ppm).expect("write");
        }
    }

    /// With screen recording permission: the region grab is larger than a thumbnail
    /// and stops at the 960 long side. Same skip as `main_display_thumbnail`.
    #[test]
    fn main_display_region_grab() {
        if !permission_granted() || !screenshots_available() {
            eprintln!("skipped: needs screen recording permission and macOS 14");
            return;
        }
        let factory = SckCaptureFactory::new();
        let id = TargetId::new(format!("{DISPLAY_PREFIX}{}", CGMainDisplayID()));
        let grab = factory
            .region_grab(&id, REGION_GRAB_LONG)
            .expect("region grab");
        let long = grab.width().max(grab.height());
        assert!(long <= REGION_GRAB_LONG);
        assert!(long > THUMBNAIL_MAX.width.max(THUMBNAIL_MAX.height));
        assert!(
            grab.rgba().chunks_exact(4).any(|p| p[..3] != [0, 0, 0]),
            "all black"
        );
    }
}

#[cfg(test)]
mod callback_tests {
    use super::*;

    #[test]
    fn completion_block_contains_a_panic() {
        let (tx, rx) = mpsc::sync_channel(1);
        let block = completion_block(tx);
        COMPLETION_PANIC.store(true, Ordering::Relaxed);
        block.call((std::ptr::null_mut(),));
        assert!(rx.try_recv().is_err());
        assert!(!COMPLETION_PANIC.load(Ordering::Relaxed));
        block.call((std::ptr::null_mut(),));
        assert!(matches!(rx.try_recv(), Ok(Ok(()))));
    }
}
