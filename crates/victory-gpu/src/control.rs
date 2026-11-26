//! Diagnostics: steering a running title. At each of the title's presents
//! the control file (`D:\victory-control.txt`, or the path in
//! `VICTORY_CONTROL`) is looked at by its modification time; when it
//! changed, its commands run, one a line, once, and the file is deleted.
//! Blank lines and lines starting with `#` are skipped; what a command did,
//! or why it did not, is logged.
//!
//! - `pix <file.wpix>`: PIX captures the next frame into the file (a run
//!   started with `VICTORY_PIX=1`, `crate::pix`).
//! - `shot [name]`: saves the frame presented next, as the window shows it
//!   without the overlay, at full size as `<name>.png` (else
//!   `shot-<frame>.png`).
//! - `dump`: saves the next frame's targets, as `VICTORY_DUMP_TARGETS` does
//!   for the frames it lists.
//! - `set NAME=VALUE`, `unset NAME`: changes a switch of `crate::live` for
//!   the rest of the run (`unset`: as if the variable were not set). Other
//!   switches are read once, at the start.
//! - `repair textures`: every texture the title fills is read from title
//!   memory and uploaded again, and every placed texture slot is due again;
//!   `repair aliases`: every copy into a view placed over a texture the host
//!   writes is made again (`crate::texture::alias`); `repair heaps`: every
//!   record of the title's descriptor heaps is compared with its host slot
//!   and made again if they differ. Each runs once, now.
//! - `overlay on|off`: the table of counters drawn over the frame
//!   (`crate::overlay`, in every build); on from the start unless
//!   `VICTORY_OVERLAY=0`.
//! - `view <n> [r|g|b|a|rgb]`, `view <0xaddress> [...]`, `view off`: presents
//!   one texture of the frame instead of the frame: the `n`-th the frame
//!   rendered to or wrote, in first-use order as `dump` numbers them, or
//!   the texture at that address. Values are shown as dumps show them
//!   (floats scaled by their highest, single channels and depth from their
//!   lowest to their highest); one channel is shown as grey.
//! - `targets`: logs the next frame's targets, with their numbers,
//!   addresses, sizes and formats.
//!
//! Files go to `VICTORY_CAPTURE`, else to the control file's folder.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

use crate::resource::HostTexture;

/// A line of the control file.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Command {
    Pix(String),
    Shot(Option<String>),
    Dump,
    /// A switch and its value; `None` for `unset`.
    Set(String, Option<String>),
    Repair(Repair),
    Overlay(bool),
    /// `None` for `view off`.
    View(Option<Wanted>),
    Targets,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Repair {
    Textures,
    Aliases,
    Heaps,
}

/// The texture `view` asked for, and what of it to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Wanted {
    target: Target,
    channel: Channel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    /// The frame's targets in first-use order.
    Index(usize),
    /// A title texture's address.
    Address(usize),
}

/// The channel shown, or red, green and blue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Channel {
    R,
    G,
    B,
    A,
    Rgb,
}

impl Channel {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Channel::R => "r",
            Channel::G => "g",
            Channel::B => "b",
            Channel::A => "a",
            Channel::Rgb => "rgb",
        }
    }
}

/// The command on `line`; `None` for a blank line or a comment.
fn parse(line: &str) -> Result<Option<Command>, String> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return Ok(None);
    }
    let (word, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    let rest = rest.trim();
    let args: Vec<&str> = rest.split_whitespace().collect();
    let command = match (word.to_ascii_lowercase().as_str(), &args[..]) {
        ("pix", [_, ..]) => Command::Pix(rest.to_owned()),
        ("pix", []) => return Err("pix needs a file name".to_owned()),
        ("shot", []) => Command::Shot(None),
        ("shot", [name]) => Command::Shot(Some(file_name(name)?)),
        ("dump", []) => Command::Dump,
        ("set", _) => {
            let (name, value) = rest.split_once('=').ok_or("set needs NAME=VALUE")?;
            let name = name.trim();
            if name.is_empty() || name.contains(char::is_whitespace) {
                return Err(format!("not a switch name: {name:?}"));
            }
            Command::Set(name.to_owned(), Some(value.trim().to_owned()))
        }
        ("unset", [name]) => Command::Set((*name).to_owned(), None),
        ("repair", ["textures"]) => Command::Repair(Repair::Textures),
        ("repair", ["aliases"]) => Command::Repair(Repair::Aliases),
        ("repair", ["heaps"]) => Command::Repair(Repair::Heaps),
        ("overlay", ["on"]) => Command::Overlay(true),
        ("overlay", ["off"]) => Command::Overlay(false),
        ("view", ["off"]) => Command::View(None),
        ("view", [target, channel @ ..]) if channel.len() <= 1 => {
            let target = match target
                .strip_prefix("0x")
                .or_else(|| target.strip_prefix("0X"))
            {
                Some(hex) => Target::Address(
                    usize::from_str_radix(hex, 16)
                        .map_err(|_| format!("not an address: {target}"))?,
                ),
                None => Target::Index(
                    target
                        .parse()
                        .map_err(|_| format!("not a target number: {target}"))?,
                ),
            };
            let channel = match channel.first().copied() {
                None | Some("rgb") => Channel::Rgb,
                Some("r") => Channel::R,
                Some("g") => Channel::G,
                Some("b") => Channel::B,
                Some("a") => Channel::A,
                Some(other) => return Err(format!("not a channel: {other} (r, g, b, a or rgb)")),
            };
            Command::View(Some(Wanted { target, channel }))
        }
        ("targets", []) => Command::Targets,
        _ => return Err(format!("unknown command: {line}")),
    };
    Ok(Some(command))
}

/// `name` if it is a plain file name (letters, digits, `-`, `_`, `.`).
fn file_name(name: &str) -> Result<String, String> {
    let plain = !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if plain {
        Ok(name.trim_end_matches(".png").to_owned())
    } else {
        Err(format!("not a plain file name: {name}"))
    }
}

/// The control file.
fn path() -> &'static Path {
    static PATH: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    PATH.get_or_init(|| {
        std::env::var_os("VICTORY_CONTROL")
            .map_or_else(|| PathBuf::from(r"D:\victory-control.txt"), PathBuf::from)
    })
}

/// Where files go: `VICTORY_CAPTURE`, else the control file's folder.
pub(crate) fn folder() -> PathBuf {
    crate::capture::folder().cloned().unwrap_or_else(|| {
        path()
            .parent()
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
    })
}

/// The control file's text if it changed since the last look; the file is
/// deleted then. One query of its time when it did not.
fn take_file() -> Option<String> {
    static LAST: Mutex<Option<std::time::SystemTime>> = Mutex::new(None);
    let modified = std::fs::metadata(path()).ok()?.modified().ok()?;
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    if *last == Some(modified) {
        return None;
    }
    let bytes = std::fs::read(path()).ok()?;
    // Noted before the delete: if that fails, the same file does not run twice.
    *last = Some(modified);
    if let Err(e) = std::fs::remove_file(path()) {
        tracing::warn!(error = %e, path = %path().display(), "the control file was not deleted");
    }
    Some(text(&bytes))
}

/// The control file's bytes as text: UTF-8, or UTF-16 with its byte order
/// mark (what `>` writes in Windows PowerShell 5.1).
fn text(bytes: &[u8]) -> String {
    if let Some(utf16) = bytes.strip_prefix(&[0xff, 0xfe]) {
        let units: Vec<u16> = utf16
            .chunks_exact(2)
            .map(|u| u16::from_le_bytes([u[0], u[1]]))
            .collect();
        return String::from_utf16_lossy(&units);
    }
    String::from_utf8_lossy(bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes)).into_owned()
}

/// Whether a view is on ([`VIEW`]), for the cheap checks.
static VIEWING: AtomicBool = AtomicBool::new(false);
static VIEW: Mutex<Option<Wanted>> = Mutex::new(None);
/// The frame whose targets `dump` saves, and the one `targets` lists
/// (`u64::MAX`: none).
static DUMP_AT: AtomicU64 = AtomicU64::new(u64::MAX);
static LIST_AT: AtomicU64 = AtomicU64::new(u64::MAX);
/// The name of the shot asked for.
static SHOT: Mutex<Option<String>> = Mutex::new(None);
/// The targets of the frame presented last ([`frame_targets`]): its number
/// and the targets, in first-use order.
static TARGETS: Mutex<(u64, crate::capture::Targets)> = Mutex::new((0, Vec::new()));

/// At the present of `frame`: runs the control file's commands if it changed.
pub(crate) fn presented(frame: u64) {
    let Some(text) = take_file() else { return };
    for line in text.lines() {
        match parse(line) {
            Ok(Some(command)) => {
                tracing::info!(frame, line = line.trim(), "control file");
                run(command, frame);
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(line = line.trim(), "control file: {e}"),
        }
    }
}

fn run(command: Command, frame: u64) {
    match command {
        Command::Pix(file) => {
            let file = PathBuf::from(file);
            let file = if file.is_absolute() {
                file
            } else {
                folder().join(file)
            };
            crate::pix::capture_next(&file);
        }
        Command::Shot(name) => {
            *SHOT.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(name.unwrap_or_else(|| format!("shot-{frame}")));
        }
        Command::Dump => DUMP_AT.store(frame + 1, Relaxed),
        Command::Set(name, value) => {
            if !crate::live::SWITCHES.contains(&name.as_str()) {
                tracing::warn!(
                    name,
                    live = ?crate::live::SWITCHES,
                    "not a live switch: noted, but what reads it read it at the start"
                );
            }
            crate::live::set(&name, value.as_deref());
        }
        Command::Repair(Repair::Textures) => {
            let staged = crate::texture::repair();
            let slots = crate::descriptor::host::repair_textures();
            tracing::info!(
                staged,
                slots_due = slots,
                "repair: textures read through tables are uploaded again from title memory by their next lists; \
                 texture slots picked by index are due again"
            );
        }
        Command::Repair(Repair::Aliases) => {
            let copies = crate::texture::repair_aliases();
            tracing::info!(copies, "repair: alias copies made again");
        }
        Command::Repair(Repair::Heaps) => {
            let started = std::time::Instant::now();
            let [looked, mirrored] = crate::descriptor::repair_heaps();
            tracing::info!(
                looked,
                mirrored,
                ms = started.elapsed().as_millis(),
                "repair: descriptor heap records compared with their host slots (those made from another record made again)"
            );
        }
        Command::Overlay(on) => crate::overlay::set_on(on),
        Command::View(wanted) => {
            *VIEW.lock().unwrap_or_else(|e| e.into_inner()) = wanted;
            VIEWING.store(wanted.is_some(), Relaxed);
        }
        Command::Targets => LIST_AT.store(frame + 1, Relaxed),
    }
}

/// Whether the targets of the frame being made are noted
/// (`crate::capture::note_target`): a view is on, or `dump` or `targets`
/// asked for them.
pub(crate) fn wants_targets(frame: u64) -> bool {
    VIEWING.load(Relaxed) || DUMP_AT.load(Relaxed) == frame || LIST_AT.load(Relaxed) == frame
}

/// Whether `dump` asked for frame `frame`'s targets.
pub(crate) fn dumps(frame: u64) -> bool {
    DUMP_AT.load(Relaxed) == frame
}

/// At the present of `frame`: the targets it rendered to or wrote, in
/// first-use order, kept for a view until the next present (the host copies
/// with them), and listed if `targets` asked.
pub(crate) fn frame_targets(frame: u64, targets: crate::capture::Targets) {
    if LIST_AT.load(Relaxed) == frame {
        tracing::info!(
            frame,
            count = targets.len(),
            "the frame's targets, in first-use order (view <number>)"
        );
        for (i, (resource, copy)) in targets.iter().enumerate() {
            // SAFETY: a live resource.
            let d = unsafe { copy.texture.GetDesc() };
            tracing::info!(
                "target {i}: {resource:#x} {}x{}x{} f{} mips {} samples {} flags {:#x}",
                d.Width,
                d.Height,
                d.DepthOrArraySize,
                d.Format.0,
                d.MipLevels,
                d.SampleDesc.Count,
                d.Flags.0
            );
        }
    }
    *TARGETS.lock().unwrap_or_else(|e| e.into_inner()) = (frame, targets);
}

/// What the present of a frame does beyond showing it.
pub(crate) struct AtPresent {
    pub(crate) overlay: bool,
    /// With a view on: the texture, or why there is none.
    pub(crate) view: Option<Result<Viewed, String>>,
    /// The name of a shot to take.
    pub(crate) shot: Option<String>,
}

/// The texture a view shows.
pub(crate) struct Viewed {
    /// How the overlay names it.
    pub(crate) label: String,
    pub(crate) copy: Arc<HostTexture>,
    pub(crate) channel: Channel,
}

/// At the host's present of a frame (`crate::host::Host::present_planes`,
/// on whichever thread presents): what to do beyond showing it; `None` when
/// nothing (two atomic loads and a lock).
pub(crate) fn at_present() -> Option<AtPresent> {
    let overlay = crate::overlay::on();
    let viewing = VIEWING.load(Relaxed);
    let shot = SHOT.lock().unwrap_or_else(|e| e.into_inner()).take();
    if !overlay && !viewing && shot.is_none() {
        return None;
    }
    let wanted = *VIEW.lock().unwrap_or_else(|e| e.into_inner());
    let view = wanted.map(resolve);
    Some(AtPresent {
        overlay,
        view,
        shot,
    })
}

/// The texture `wanted` names among the targets of the frame the title
/// presented last (with presents sent to a thread of ours, the host's
/// present may come a frame or so later).
fn resolve(wanted: Wanted) -> Result<Viewed, String> {
    let targets = TARGETS.lock().unwrap_or_else(|e| e.into_inner());
    let (index, resource, copy) = match wanted.target {
        Target::Index(i) => {
            let (resource, copy) = targets.1.get(i).ok_or_else(|| {
                format!(
                    "view {i}: frame {} has {} targets",
                    targets.0,
                    targets.1.len()
                )
            })?;
            (Some(i), *resource, copy.clone())
        }
        Target::Address(at) => match targets.1.iter().position(|(r, _)| *r == at) {
            Some(i) => (Some(i), at, targets.1[i].1.clone()),
            // Any title texture alive.
            None => {
                let copy = crate::resource::texture_alive(at)
                    .ok_or_else(|| format!("view {at:#x}: not a texture with a host copy"))?;
                (None, at, copy)
            }
        },
    };
    // SAFETY: a live resource.
    let d = unsafe { copy.texture.GetDesc() };
    let number = index.map_or_else(String::new, |i| format!("{i} "));
    Ok(Viewed {
        label: format!(
            "view {number}{resource:#x} {}x{} f{} {}",
            d.Width,
            d.Height,
            d.Format.0,
            wanted.channel.name()
        ),
        copy,
        channel: wanted.channel,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(line: &str) -> Command {
        parse(line)
            .unwrap_or_else(|e| panic!("{line}: {e}"))
            .unwrap_or_else(|| panic!("{line}: nothing"))
    }

    #[test]
    fn commands_parse() {
        assert_eq!(
            ok("pix D:\\caps\\cell.wpix"),
            Command::Pix("D:\\caps\\cell.wpix".to_owned())
        );
        assert_eq!(ok("shot"), Command::Shot(None));
        assert_eq!(
            ok("shot cell-1.png"),
            Command::Shot(Some("cell-1".to_owned()))
        );
        assert_eq!(ok("  dump  "), Command::Dump);
        assert_eq!(
            ok("set VICTORY_HEAP_RECHECK = 0"),
            Command::Set("VICTORY_HEAP_RECHECK".to_owned(), Some("0".to_owned()))
        );
        assert_eq!(
            ok("set VICTORY_SKIP_PIPELINES="),
            Command::Set("VICTORY_SKIP_PIPELINES".to_owned(), Some(String::new()))
        );
        assert_eq!(
            ok("unset VICTORY_HEAP_SYNC"),
            Command::Set("VICTORY_HEAP_SYNC".to_owned(), None)
        );
        assert_eq!(ok("repair textures"), Command::Repair(Repair::Textures));
        assert_eq!(ok("repair aliases"), Command::Repair(Repair::Aliases));
        assert_eq!(ok("repair heaps"), Command::Repair(Repair::Heaps));
        assert_eq!(ok("overlay on"), Command::Overlay(true));
        assert_eq!(ok("OVERLAY off"), Command::Overlay(false));
        assert_eq!(ok("view off"), Command::View(None));
        assert_eq!(
            ok("view 12"),
            Command::View(Some(Wanted {
                target: Target::Index(12),
                channel: Channel::Rgb
            }))
        );
        assert_eq!(
            ok("view 0x1f2e3d40 a"),
            Command::View(Some(Wanted {
                target: Target::Address(0x1f2e_3d40),
                channel: Channel::A
            }))
        );
        assert_eq!(ok("targets"), Command::Targets);
    }

    /// What editors and shells write: UTF-8 with or without a byte order
    /// mark, UTF-16 with one (Windows PowerShell 5.1's `>`).
    #[test]
    fn the_file_is_read_in_the_encodings_shells_write() {
        assert_eq!(text(b"view 3\r\n"), "view 3\r\n");
        assert_eq!(text(b"\xef\xbb\xbfview 3"), "view 3");
        let utf16: Vec<u8> = [0xff, 0xfe]
            .into_iter()
            .chain("overlay on\r\n".encode_utf16().flat_map(u16::to_le_bytes))
            .collect();
        assert_eq!(text(&utf16), "overlay on\r\n");
        let first = text(&utf16).lines().next().map(str::to_owned);
        assert_eq!(
            parse(first.as_deref().unwrap_or_default()),
            Ok(Some(Command::Overlay(true)))
        );
    }

    #[test]
    fn blank_lines_and_comments_are_skipped() {
        assert_eq!(parse(""), Ok(None));
        assert_eq!(parse("   "), Ok(None));
        assert_eq!(parse("# view 3"), Ok(None));
    }

    #[test]
    fn bad_lines_are_refused() {
        for line in [
            "pix",
            "shot ../x",
            "shot a b",
            "set NOVALUE",
            "set =1",
            "unset",
            "repair everything",
            "overlay maybe",
            "view",
            "view x",
            "view 0xzz",
            "view 3 q",
            "view 3 r g",
            "dump now",
            "fly",
        ] {
            assert!(parse(line).is_err(), "{line} was taken");
        }
    }
}
