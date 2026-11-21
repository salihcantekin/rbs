//! Copies CEF's accelerated-paint texture into the studio's textures (ADR-0109
//! decision 8, ADR-0157). The callback never waits on the CPU: it claims a free
//! slot, records the copy, signals the studio's shared fence, flushes and publishes.
//! The studio's supervisor hands the picture on only once that fence value has
//! completed, so neither side ever blocks on the other.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rbs_browser_ipc::D3dShare;
use rbs_browser_shm::FrameRing;
use windows::Win32::Foundation::{HANDLE, HMODULE};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BOX, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice,
    ID3D11Device1, ID3D11Device5, ID3D11DeviceContext4, ID3D11Fence, ID3D11Multithread,
    ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory4, IDXGIKeyedMutex};
use windows::core::Interface;

/// What one accelerated paint did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Copied {
    /// The copy is recorded and the slot published.
    Published,
    /// Every slot was held: the frame is dropped (not counted as a failure).
    NoSlot,
    /// CEF's texture has a keyed mutex that was not free at once.
    Busy,
    /// The copy or the signal failed.
    Failed,
    /// CEF's texture did not open on the studio's adapter: fall back at once.
    OpenFailed,
}

/// Which key, if any, guards CEF's texture. Found on the first paint.
const MUTEX_UNKNOWN: u32 = 0;
const MUTEX_NONE: u32 = 1;
/// `MUTEX_KEY0 + key`: the key the consumer acquires.
const MUTEX_KEY0: u32 = 2;

pub struct Copier {
    device: ID3D11Device1,
    context: ID3D11DeviceContext4,
    fence: ID3D11Fence,
    slots: Vec<ID3D11Texture2D>,
    value: AtomicU64,
    mutex: AtomicU32,
    stats: Option<Stats>,
}

// SAFETY: CEF calls `copy` only on its UI thread, which is the thread that runs the
// runner's message loop, opens the copier and drops it at exit; the COM objects are
// therefore never used from two threads. The static `OnceLock` needs `Sync` only to
// exist; no other thread reads it.
unsafe impl Send for Copier {}
// SAFETY: as `Send`: one thread uses the value.
unsafe impl Sync for Copier {}

impl Copier {
    /// Opens the studio's textures and fence on the adapter the studio named.
    /// `None` when this process cannot see them: the tree falls back.
    pub fn open(share: &D3dShare) -> Option<Self> {
        let adapter = adapter(share.adapter_luid)?;
        let mut device = None;
        let mut immediate = None;
        // SAFETY: valid out pointers; the adapter is live for the call.
        unsafe {
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut immediate),
            )
            .ok()?;
        }
        let device = device?;
        let immediate = immediate?;
        let multithread: ID3D11Multithread = device.cast().ok()?;
        // SAFETY: plain call on a live device.
        unsafe {
            let _ = multithread.SetMultithreadProtected(true);
        }
        let device1: ID3D11Device1 = device.cast().ok()?;
        let device5: ID3D11Device5 = device.cast().ok()?;
        let context: ID3D11DeviceContext4 = immediate.cast().ok()?;
        let mut fence = None;
        // SAFETY: the handle is the studio's fence, duplicated into this process.
        unsafe {
            device5
                .OpenSharedFence(HANDLE(share.fence as *mut std::ffi::c_void), &mut fence)
                .ok()?;
        }
        let fence = fence?;
        let mut slots = Vec::with_capacity(share.handles().len());
        for &handle in share.handles() {
            // SAFETY: the handle is a studio texture duplicated into this process.
            let texture: ID3D11Texture2D = unsafe {
                device1
                    .OpenSharedResource1(HANDLE(handle as *mut std::ffi::c_void))
                    .ok()?
            };
            slots.push(texture);
        }
        if slots.is_empty() {
            return None;
        }
        Some(Self {
            device: device1,
            context,
            fence,
            slots,
            value: AtomicU64::new(0),
            mutex: AtomicU32::new(MUTEX_UNKNOWN),
            stats: std::env::var_os("RBS_BROWSER_GPU_STATS").map(|_| Stats::default()),
        })
    }

    /// Copies `source` (CEF's handle, valid for this callback) into a free studio
    /// slot, signals and publishes it. Never waits (ADR-0157 decision 1).
    pub fn copy(&self, source: *mut std::ffi::c_void, ring: &FrameRing) -> Copied {
        let Some(slot) = ring.claim_gpu() else {
            return Copied::NoSlot;
        };
        let Some(dest) = self.slots.get(slot as usize) else {
            ring.abandon_gpu(slot);
            return Copied::Failed;
        };
        let start = Instant::now();
        // CEF's handle value may name another texture later, so it is opened per
        // callback and never cached (ADR-0157 decision 7).
        // SAFETY: CEF passes a shared handle that stays valid for this callback; we do
        // not close it.
        let opened: windows::core::Result<ID3D11Texture2D> =
            unsafe { self.device.OpenSharedResource1(HANDLE(source)) };
        let Ok(texture) = opened else {
            ring.abandon_gpu(slot);
            return Copied::OpenFailed;
        };
        let opened_at = Instant::now();
        let Ok(mutex) = self.acquire(&texture) else {
            ring.abandon_gpu(slot);
            return Copied::Busy;
        };
        let value = self.value.fetch_add(1, Ordering::Relaxed) + 1;
        // SAFETY: both textures are live on this device; the source box is inside
        // the source (checked first). No call here waits for the GPU.
        let signalled = unsafe {
            let mut src_desc = std::mem::zeroed();
            let mut dst_desc = std::mem::zeroed();
            texture.GetDesc(&mut src_desc);
            dest.GetDesc(&mut dst_desc);
            let fits = src_desc.Width >= dst_desc.Width && src_desc.Height >= dst_desc.Height;
            if fits && src_desc.Width == dst_desc.Width && src_desc.Height == dst_desc.Height {
                self.context.CopyResource(dest, &texture);
            } else if fits {
                let region = D3D11_BOX {
                    left: 0,
                    top: 0,
                    front: 0,
                    right: dst_desc.Width,
                    bottom: dst_desc.Height,
                    back: 1,
                };
                self.context
                    .CopySubresourceRegion(dest, 0, 0, 0, 0, &texture, 0, Some(&region));
            }
            fits && self.context.Signal(&self.fence, value).is_ok()
        };
        let copied_at = Instant::now();
        // SAFETY: plain call on the live immediate context. It submits and does not
        // wait for completion (a throttling driver may still block, so it is timed).
        unsafe {
            self.context.Flush();
        }
        if let Some((mutex, key)) = mutex {
            // SAFETY: the mutex belongs to `texture`, live for this call, and this
            // thread acquired `key` above.
            let _ = unsafe { mutex.ReleaseSync(key) };
        }
        let flushed_at = Instant::now();
        if !signalled {
            ring.abandon_gpu(slot);
            return Copied::Failed;
        }
        ring.publish_gpu(slot, value);
        if let Some(stats) = &self.stats {
            stats.add(
                opened_at - start,
                copied_at - opened_at,
                flushed_at - copied_at,
            );
        }
        Copied::Published
    }

    /// The keyed mutex of CEF's texture, acquired with a zero timeout, or `None`
    /// when the texture has none. `Err` when it has one that is not free now. The
    /// first paint logs which case it is (ADR-0157 decision 3).
    #[allow(clippy::type_complexity)]
    fn acquire(&self, texture: &ID3D11Texture2D) -> Result<Option<(IDXGIKeyedMutex, u64)>, ()> {
        let known = self.mutex.load(Ordering::Relaxed);
        if known == MUTEX_NONE {
            return Ok(None);
        }
        let Ok(mutex) = texture.cast::<IDXGIKeyedMutex>() else {
            if known == MUTEX_UNKNOWN {
                eprintln!("rbs-browser-cef: CEF's paint texture has no keyed mutex");
                self.mutex.store(MUTEX_NONE, Ordering::Relaxed);
            }
            return Ok(None);
        };
        let first = [0, 1];
        let found = [u64::from(known.saturating_sub(MUTEX_KEY0))];
        let keys: &[u64] = if known == MUTEX_UNKNOWN {
            &first
        } else {
            &found
        };
        for &key in keys {
            // SAFETY: the mutex belongs to `texture`, live for this call; a zero
            // timeout never waits.
            if unsafe { mutex.AcquireSync(key, 0) }.is_ok() {
                if known == MUTEX_UNKNOWN {
                    eprintln!("rbs-browser-cef: CEF's paint texture has a keyed mutex, key {key}");
                    self.mutex.store(MUTEX_KEY0 + key as u32, Ordering::Relaxed);
                }
                return Ok(Some((mutex, key)));
            }
        }
        Err(())
    }
}

/// Callback times, printed every 120 published frames when `RBS_BROWSER_GPU_STATS`
/// is set (ADR-0157 decision 11). Off by default.
#[derive(Default)]
struct Stats {
    frames: AtomicU32,
    open_ns: AtomicU64,
    copy_ns: AtomicU64,
    flush_ns: AtomicU64,
    open_max: AtomicU64,
    copy_max: AtomicU64,
    flush_max: AtomicU64,
}

impl Stats {
    fn add(&self, open: Duration, copy: Duration, flush: Duration) {
        let parts = [
            (&self.open_ns, &self.open_max, open),
            (&self.copy_ns, &self.copy_max, copy),
            (&self.flush_ns, &self.flush_max, flush),
        ];
        for (sum, max, d) in parts {
            let ns = u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
            sum.fetch_add(ns, Ordering::Relaxed);
            max.fetch_max(ns, Ordering::Relaxed);
        }
        let frames = self.frames.fetch_add(1, Ordering::Relaxed) + 1;
        if frames < 120 {
            return;
        }
        let take = |a: &AtomicU64| a.swap(0, Ordering::Relaxed);
        let us = |ns: u64| ns as f64 / 1000.0;
        let n = u64::from(frames);
        eprintln!(
            "rbs-browser-cef gpu: {frames} frames, mean/max us: open {:.0}/{:.0} copy {:.0}/{:.0} flush {:.0}/{:.0}",
            us(take(&self.open_ns) / n),
            us(take(&self.open_max)),
            us(take(&self.copy_ns) / n),
            us(take(&self.copy_max)),
            us(take(&self.flush_ns) / n),
            us(take(&self.flush_max)),
        );
        self.frames.store(0, Ordering::Relaxed);
    }
}

fn adapter(luid: u64) -> Option<windows::Win32::Graphics::Dxgi::IDXGIAdapter> {
    let id = windows::Win32::Foundation::LUID {
        LowPart: luid as u32,
        HighPart: (luid >> 32) as i32,
    };
    // SAFETY: the factory enumerates one adapter.
    unsafe {
        let factory: IDXGIFactory4 = CreateDXGIFactory1().ok()?;
        factory.EnumAdapterByLuid(id).ok()
    }
}
