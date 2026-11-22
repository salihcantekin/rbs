//! Every thread's stack, for the watchdog's report (Windows x64 only).
//!
//! Low-level module of the test binary (CLAUDE.md rule 7): it suspends each other
//! thread of the process in turn, reads its registers, unwinds its stack with the
//! system's unwind tables into a fixed array, resumes it, and only then names the
//! frames with `dbghelp` (part of Windows, reads the PDB next to the test binary). No
//! debugger to install. A suspended thread may hold the heap lock, so nothing between
//! suspend and resume allocates or locks.
//!
//! Used only when the run is already stuck; the watchdog runs it on a thread of its own
//! and stops waiting for it after a hang guard.

use std::fmt::Write as _;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, LocalFree};
use windows_sys::Win32::System::Diagnostics::Debug::{
    CONTEXT, CONTEXT_FULL_AMD64, GetThreadContext, IMAGEHLP_LINEW64, RtlLookupFunctionEntry,
    RtlVirtualUnwind, SYMBOL_INFOW, SYMOPT_DEFERRED_LOADS, SYMOPT_LOAD_LINES, SYMOPT_UNDNAME,
    SymFromAddrW, SymGetLineFromAddrW64, SymInitializeW, SymRefreshModuleList, SymSetOptions,
    UNW_FLAG_NHANDLER,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, GetCurrentThreadId, GetThreadDescription, OpenThread,
    ResumeThread, SuspendThread, THREAD_GET_CONTEXT, THREAD_QUERY_LIMITED_INFORMATION,
    THREAD_SUSPEND_RESUME,
};

/// Frames kept per thread.
const FRAMES: usize = 48;

/// Threads listed.
const THREADS: usize = 1024;

/// `GetThreadContext` needs a 16-byte aligned `CONTEXT`.
#[repr(C, align(16))]
struct Aligned(CONTEXT);

struct Walked {
    id: u32,
    frames: [u64; FRAMES],
    len: usize,
}

/// Every other thread's stack as text: name, id and frames, innermost first.
pub(crate) fn all() -> String {
    let ids = thread_ids();
    let mut walked = Vec::with_capacity(ids.len());
    for id in ids {
        walked.push(Walked {
            id,
            frames: [0; FRAMES],
            len: 0,
        });
    }
    for w in &mut walked {
        walk(w);
    }
    let mut out = String::new();
    // SAFETY: the pseudo handle of this process is always valid. dbghelp is not
    // thread-safe; only this thread calls it now (the panic hook's backtrace runs on
    // test threads, which are stuck). A failed init (std initialised it already)
    // leaves the earlier session usable; the refresh adds modules loaded since.
    unsafe {
        let process = GetCurrentProcess();
        SymSetOptions(SYMOPT_UNDNAME | SYMOPT_DEFERRED_LOADS | SYMOPT_LOAD_LINES);
        let _ = SymInitializeW(process, std::ptr::null(), 1);
        let _ = SymRefreshModuleList(process);
    }
    for w in &walked {
        let _ = writeln!(out, "  thread {} {}:", w.id, name(w.id));
        for &ip in &w.frames[..w.len] {
            let _ = writeln!(out, "    {}", symbol(ip));
        }
    }
    out
}

/// The ids of this process's threads except the calling one.
fn thread_ids() -> Vec<u32> {
    let mut ids = Vec::new();
    // SAFETY: plain Win32 calls with a correctly sized THREADENTRY32; the snapshot
    // handle is closed before returning.
    unsafe {
        let me = GetCurrentThreadId();
        let pid = GetCurrentProcessId();
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snap == INVALID_HANDLE_VALUE {
            return ids;
        }
        let mut entry = THREADENTRY32 {
            dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        let mut ok = Thread32First(snap, &mut entry) != 0;
        while ok && ids.len() < THREADS {
            if entry.th32OwnerProcessID == pid && entry.th32ThreadID != me {
                ids.push(entry.th32ThreadID);
            }
            ok = Thread32Next(snap, &mut entry) != 0;
        }
        CloseHandle(snap);
    }
    ids
}

/// Suspends thread `w.id`, unwinds it into `w.frames`, resumes it.
fn walk(w: &mut Walked) {
    // SAFETY: the thread handle is checked and closed. Between SuspendThread and
    // ResumeThread nothing allocates or takes a lock this code owns: the context is on
    // this stack, the frames go into a fixed array, and RtlLookupFunctionEntry and
    // RtlVirtualUnwind read the loaded images' unwind tables. Reading a suspended
    // thread's stack memory through RtlVirtualUnwind is what debuggers do; a thread
    // that is gone fails OpenThread or GetThreadContext and is skipped.
    unsafe {
        let thread: HANDLE = OpenThread(
            THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT | THREAD_QUERY_LIMITED_INFORMATION,
            0,
            w.id,
        );
        if thread.is_null() {
            return;
        }
        if SuspendThread(thread) == u32::MAX {
            CloseHandle(thread);
            return;
        }
        let mut ctx: Aligned = std::mem::zeroed();
        ctx.0.ContextFlags = CONTEXT_FULL_AMD64;
        if GetThreadContext(thread, &mut ctx.0) != 0 {
            let c = &mut ctx.0;
            while w.len < FRAMES && c.Rip != 0 {
                w.frames[w.len] = c.Rip;
                w.len += 1;
                let mut base = 0u64;
                let entry = RtlLookupFunctionEntry(c.Rip, &mut base, std::ptr::null_mut());
                if entry.is_null() {
                    // A leaf function: the return address is on top of the stack.
                    if c.Rsp == 0 {
                        break;
                    }
                    c.Rip = *(c.Rsp as *const u64);
                    c.Rsp += 8;
                } else {
                    let mut data = std::ptr::null_mut();
                    let mut frame = 0u64;
                    RtlVirtualUnwind(
                        UNW_FLAG_NHANDLER,
                        base,
                        c.Rip,
                        entry,
                        c,
                        &mut data,
                        &mut frame,
                        std::ptr::null_mut(),
                    );
                }
            }
        }
        ResumeThread(thread);
        CloseHandle(thread);
    }
}

/// The thread's description (the Rust thread name), or empty.
fn name(id: u32) -> String {
    // SAFETY: the handle is checked and closed; GetThreadDescription allocates the
    // returned string with LocalAlloc, freed here after copying it.
    unsafe {
        let thread = OpenThread(THREAD_QUERY_LIMITED_INFORMATION, 0, id);
        if thread.is_null() {
            return String::new();
        }
        let mut text = std::ptr::null_mut();
        let mut name = String::new();
        if GetThreadDescription(thread, &mut text) >= 0 && !text.is_null() {
            let mut len = 0;
            while *text.add(len) != 0 {
                len += 1;
            }
            name = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
            LocalFree(text.cast());
        }
        CloseHandle(thread);
        name
    }
}

/// `module!function file:line` for an instruction address, or the bare address.
fn symbol(ip: u64) -> String {
    const NAME: usize = 512;
    #[repr(C)]
    struct Info {
        info: SYMBOL_INFOW,
        rest: [u16; NAME],
    }
    // SAFETY: `Info` is a SYMBOL_INFOW followed by room for `NAME` more characters,
    // as MaxNameLen says; the line structure is sized; dbghelp writes only inside them.
    unsafe {
        let process = GetCurrentProcess();
        let mut info: Info = std::mem::zeroed();
        info.info.SizeOfStruct = std::mem::size_of::<SYMBOL_INFOW>() as u32;
        info.info.MaxNameLen = NAME as u32;
        let mut displacement = 0u64;
        // A return address points after the call; look up the call itself.
        let at = ip.saturating_sub(1);
        let mut out = if SymFromAddrW(process, at, &mut displacement, &mut info.info) != 0 {
            let len = (info.info.NameLen as usize).min(NAME);
            let name = std::slice::from_raw_parts(info.info.Name.as_ptr(), len);
            String::from_utf16_lossy(name)
        } else {
            format!("{ip:#x}")
        };
        let mut line: IMAGEHLP_LINEW64 = std::mem::zeroed();
        line.SizeOfStruct = std::mem::size_of::<IMAGEHLP_LINEW64>() as u32;
        let mut col = 0u32;
        if SymGetLineFromAddrW64(process, at, &mut col, &mut line) != 0 && !line.FileName.is_null()
        {
            let mut len = 0;
            while *line.FileName.add(len) != 0 {
                len += 1;
            }
            let file = String::from_utf16_lossy(std::slice::from_raw_parts(line.FileName, len));
            let _ = write!(out, " ({file}:{})", line.LineNumber);
        }
        out
    }
}
