//! Diagnostics: GPU captures of the host device with Microsoft PIX, taken
//! from inside the run (our launcher starts the title, not PIX).
//! `VICTORY_PIX_CAPTURE=<frame>:<file.wpix>` loads PIX's capturer before the
//! host device is made and captures that presented frame into the file, to
//! open in PIX (every draw and dispatch, its inputs and outputs).
//! `VICTORY_PIX=1` loads the capturer with no frame named: `pix <file.wpix>`
//! in the control file (`crate::control`) captures the next frame. The
//! capturer must wrap the device from its creation, so it cannot be loaded
//! later. `VICTORY_PIX_DLL` names the capturer; else the newest installed
//! PIX's.

use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

/// The frame to capture and the capture file.
fn wanted() -> Option<&'static (u64, Vec<u16>)> {
    static WANTED: OnceLock<Option<(u64, Vec<u16>)>> = OnceLock::new();
    WANTED
        .get_or_init(|| {
            let v = std::env::var("VICTORY_PIX_CAPTURE").ok()?;
            let (frame, file) = v.split_once(':')?;
            Some((frame.trim().parse().ok()?, wide(Path::new(file.trim()))))
        })
        .as_ref()
}

/// `VICTORY_PIX=1`: the capturer is loaded for captures the control file asks for.
fn on_call() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_PIX").is_some_and(|v| v == "1"))
}

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain([0]).collect()
}

/// The capturer's module, loaded at most once.
fn module() -> Option<isize> {
    static MODULE: OnceLock<Option<isize>> = OnceLock::new();
    *MODULE.get_or_init(|| {
        let dll = std::env::var_os("VICTORY_PIX_DLL")
            .map(PathBuf::from)
            .or_else(|| {
                let mut found: Vec<PathBuf> = std::fs::read_dir(r"C:\Program Files\Microsoft PIX")
                    .ok()?
                    .filter_map(|e| Some(e.ok()?.path().join("WinPixGpuCapturer.dll")))
                    .filter(|p| p.exists())
                    .collect();
                found.sort();
                found.pop()
            })?;
        let wide = wide(&dll);
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

/// Whether the capturer is wanted (it then wraps the host device).
pub(crate) fn active() -> bool {
    wanted().is_some() || on_call()
}

/// Before the host device is made: loads the capturer when it is wanted.
pub(crate) fn before_device() {
    if active() {
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
    if let Some(hr) = capture(file) {
        tracing::info!(
            frame = at,
            hr = format_args!("{hr:#x}"),
            "PIX capture asked for"
        );
    }
}

/// Asks PIX to capture the next frame into `file` (`pix` in the control
/// file); false if the capturer is not loaded.
pub(crate) fn capture_next(file: &Path) -> bool {
    if !active() || module().is_none() {
        tracing::warn!(
            "no PIX capture: the capturer is loaded only in runs started with VICTORY_PIX=1"
        );
        return false;
    }
    let hr = capture(&wide(file));
    tracing::info!(file = %file.display(), hr = format_args!("{:#x}", hr.unwrap_or(-1)), "PIX capture of the next frame asked for");
    hr.is_some_and(|hr| hr >= 0)
}

/// `CaptureNextFrame` of the capturer, into `file` (null-terminated): its
/// result.
fn capture(file: &[u16]) -> Option<i32> {
    let module = module()?;
    // SAFETY: the module is loaded and stays so; the name is null-terminated.
    let Some(capture) =
        (unsafe { GetProcAddress(module as _, c"CaptureNextFrame".as_ptr().cast()) })
    else {
        tracing::warn!("PIX's capturer has no CaptureNextFrame");
        return None;
    };
    // SAFETY: PIX's `CaptureNextFrame(PCWSTR file, UINT32 frames)`.
    let capture: extern "system" fn(*const u16, u32) -> i32 =
        unsafe { std::mem::transmute(capture) };
    Some(capture(file.as_ptr(), 1))
}
