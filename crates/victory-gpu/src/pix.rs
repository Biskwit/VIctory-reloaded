//! Diagnostics: GPU captures of the host device with Microsoft PIX, taken
//! from inside the run (our launcher starts the title, not PIX).
//! `VICTORY_PIX_CAPTURE=<frame>:<file.wpix>` loads PIX's capturer before the
//! host device is made and captures that presented frame into the file, to
//! open in PIX (every draw and dispatch, its inputs and outputs).
//! `VICTORY_PIX_DLL` names the capturer; else the newest installed PIX's.

use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::OnceLock;

use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

/// The frame to capture and the capture file.
fn wanted() -> Option<&'static (u64, Vec<u16>)> {
    static WANTED: OnceLock<Option<(u64, Vec<u16>)>> = OnceLock::new();
    WANTED
        .get_or_init(|| {
            let v = std::env::var("VICTORY_PIX_CAPTURE").ok()?;
            let (frame, file) = v.split_once(':')?;
            let file: Vec<u16> = std::ffi::OsStr::new(file.trim()).encode_wide().chain([0]).collect();
            Some((frame.trim().parse().ok()?, file))
        })
        .as_ref()
}

/// The capturer's module, loaded at most once.
fn module() -> Option<isize> {
    static MODULE: OnceLock<Option<isize>> = OnceLock::new();
    *MODULE.get_or_init(|| {
        let dll = std::env::var_os("VICTORY_PIX_DLL").map(PathBuf::from).or_else(|| {
            let mut found: Vec<PathBuf> = std::fs::read_dir(r"C:\Program Files\Microsoft PIX")
                .ok()?
                .filter_map(|e| Some(e.ok()?.path().join("WinPixGpuCapturer.dll")))
                .filter(|p| p.exists())
                .collect();
            found.sort();
            found.pop()
        })?;
        let wide: Vec<u16> = dll.as_os_str().encode_wide().chain([0]).collect();
        // SAFETY: a null-terminated path.
        let module = unsafe { LoadLibraryW(wide.as_ptr()) };
        if module.is_null() {
            tracing::warn!(dll = %dll.display(), "PIX's capturer did not load");
            return None;
        }
        tracing::info!(dll = %dll.display(), "PIX's capturer loaded");
        Some(module as isize)
    })
}

/// Whether a capture is wanted (the capturer then wraps the host device).
pub(crate) fn active() -> bool {
    wanted().is_some()
}

/// Before the host device is made: loads the capturer when a capture is wanted.
pub(crate) fn before_device() {
    if wanted().is_some() {
        let _ = module();
    }
}

/// At the present of `frame`: asks for the next frame's capture when that
/// is the one wanted.
pub(crate) fn presented(frame: u64) {
    let Some((at, file)) = wanted() else { return };
    if frame + 1 != *at {
        return;
    }
    let Some(module) = module() else { return };
    // SAFETY: the module is loaded and stays so; the name is null-terminated.
    let Some(capture) = (unsafe { GetProcAddress(module as _, c"CaptureNextFrame".as_ptr().cast()) }) else {
        tracing::warn!("PIX's capturer has no CaptureNextFrame");
        return;
    };
    // SAFETY: PIX's `CaptureNextFrame(PCWSTR file, UINT32 frames)`.
    let capture: extern "system" fn(*const u16, u32) -> i32 = unsafe { std::mem::transmute(capture) };
    let hr = capture(file.as_ptr(), 1);
    tracing::info!(frame = at, hr = format_args!("{hr:#x}"), "PIX capture asked for");
}
