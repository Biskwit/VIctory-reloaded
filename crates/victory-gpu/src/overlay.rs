//! What the host draws over the presented frame, after the title's planes
//! are copied or composed into the window's buffer and before it is
//! presented (`crate::host::Host::present_planes`, on whichever thread
//! presents): the overlay, in every build, on unless `VICTORY_OVERLAY=0`.
//! Saved frames and the frame recorder read the title's picture first and
//! never see it. (`VICTORY_PANEL`'s panel, `crate::host::panel`, is another
//! thing, and is drawn after it.)
//!
//! It is a compact table on a dark panel at the top left: the console the
//! title sees; frames a second and time a frame in large type, green from
//! the 15 a second aimed at, amber from 10, red below; where a frame's time
//! goes, with bars of its share of the frame; draws, dropped draws and the
//! card's memory, a grey label on the left and its value in white against
//! the right edge. Diagnostics builds add a block in smaller type: the
//! submitting thread's waits for the card, dispatches, the shader and
//! pipeline caches, shared memory, dynamic descriptor slots, the live
//! switches set (`crate::live`) and the view's line ([`view`]); there
//! `overlay on|off` of `crate::control` shows and hides it too.
//!
//! Text is Consolas, which ships with Windows, drawn by GDI with grey
//! anti-aliasing into an atlas when the overlay first draws (and again when
//! the window's height changes), at sizes that follow the window's height;
//! the shader blends its ink by the atlas's coverage, a pixel for a pixel.
//! Without Consolas our own 5x8 bitmap font takes its place.
//!
//! The shaders are `overlay.hlsl`, compiled when the overlay first draws
//! (as `crate::compose`'s are). The rows are made when the frame line's
//! figures change, once a second (`crate::host`'s meter); every present
//! replays their draws, a rectangle each: the panel, its lines and bars,
//! and a draw a label or value.

use std::sync::atomic::{AtomicU8, Ordering::Relaxed};

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP, ID3DBlob};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::{PCSTR, s};

use crate::host::Host;
#[cfg(not(feature = "diagnostics"))]
use crate::host::HostList;

#[cfg(feature = "diagnostics")]
pub(crate) mod view;

const SOURCE: &str = include_str!("overlay.hlsl");

/// The frame line's figures over its last second (`crate::host`'s meter):
/// frames a second; milliseconds a frame, of them preparing submissions,
/// the card busy with ours (the card's clock: something of ours queued),
/// the submitting thread waiting for the card, and copying back; draws,
/// dispatches and drops a frame; in diagnostics builds, dynamic descriptor
/// slots taken a frame and tables refused them.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct FrameLine {
    pub(crate) fps: f64,
    pub(crate) ms: f64,
    pub(crate) cpu: f64,
    pub(crate) busy: f64,
    pub(crate) wait: f64,
    pub(crate) back: f64,
    pub(crate) draws: f64,
    pub(crate) dispatches: f64,
    pub(crate) dropped: f64,
    pub(crate) dynamic: [f64; 2],
}

/// Characters of a line at most: the shader's eight words of four.
const LINE: usize = 128;

/// The shaders' constants (`Draw` in overlay.hlsl).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Constants {
    rect: [f32; 4],
    target: [f32; 2],
    style: [u32; 2],
    head: [u32; 4],
    words: [u32; LINE / 4],
}

/// The root parameters: the constants (both root signatures), the atlas.
const DRAW: u32 = 0;
const FONT: u32 = 1;

/// The atlas's faces: the big figures, the rows, the diagnostics block.
const BIG: usize = 0;
const MAIN: usize = 1;
const SMALL: usize = 2;

/// Pixels an em of each face in a window 1080 pixels high; more or fewer
/// with the window's height.
const EMS: [u32; 3] = [24, 14, 12];

/// A face of the atlas: the cell of its glyphs in pixels (one for all: the
/// fonts are monospaced) and the byte where its first glyph starts.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Face {
    cell: [u32; 2],
    base: u32,
}

/// The table's width in characters of the main face at least, a bar's,
/// and the value column beside bars ("100.0 ms").
const NARROWEST: usize = 26;
const BAR_CHARS: usize = 8;
const BAR_VALUE: usize = 8;

/// A colour as the shader takes it: 8 bits a channel, red lowest, alpha
/// highest.
const fn rgba(r: u8, g: u8, b: u8, a: u8) -> u32 {
    u32::from_le_bytes([r, g, b, a])
}

const BACKGROUND: u32 = rgba(10, 12, 16, 216);
const RULE: u32 = rgba(255, 255, 255, 40);
const TRACK: u32 = rgba(255, 255, 255, 28);
const BAR: u32 = rgba(96, 165, 250, 255);
const LABEL: u32 = rgba(150, 158, 170, 255);
const VALUE: u32 = rgba(240, 242, 245, 255);
const DETAIL: u32 = rgba(196, 202, 210, 255);
const GREEN: u32 = rgba(90, 255, 115, 255);
const AMBER: u32 = rgba(255, 199, 64, 255);
const RED: u32 = rgba(255, 89, 77, 255);

/// The overlay: 0 not looked at yet, 1 off, 2 on.
static ON: AtomicU8 = AtomicU8::new(0);

/// Whether the overlay is drawn: from the start unless `VICTORY_OVERLAY=0`.
pub(crate) fn on() -> bool {
    match ON.load(Relaxed) {
        0 => {
            let on = std::env::var_os("VICTORY_OVERLAY").is_none_or(|v| v != "0");
            let _ = ON.compare_exchange(0, if on { 2 } else { 1 }, Relaxed, Relaxed);
            ON.load(Relaxed) == 2
        }
        state => state == 2,
    }
}

/// Shows or hides the overlay (`overlay on|off`).
#[cfg(feature = "diagnostics")]
pub(crate) fn set_on(on: bool) {
    ON.store(if on { 2 } else { 1 }, Relaxed);
}

/// The glyphs of characters 32 (space) to 126 (`~`), drawn for this
/// overlay where Windows has no Consolas ([`bitmap`]): 8 rows a glyph, top
/// first, its 5 columns in bits 4 (left) to 0.
/// Capitals and digits stand on rows 0-6; row 7 holds descenders.
#[rustfmt::skip]
const GLYPHS: [[u8; 8]; 95] = [
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // space
    [0x04, 0x04, 0x04, 0x04, 0x04, 0x00, 0x04, 0x00], // !
    [0x0a, 0x0a, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // "
    [0x0a, 0x0a, 0x1f, 0x0a, 0x1f, 0x0a, 0x0a, 0x00], // #
    [0x04, 0x0f, 0x14, 0x0e, 0x05, 0x1e, 0x04, 0x00], // $
    [0x18, 0x19, 0x02, 0x04, 0x08, 0x13, 0x03, 0x00], // %
    [0x0c, 0x12, 0x14, 0x08, 0x15, 0x12, 0x0d, 0x00], // &
    [0x04, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // '
    [0x02, 0x04, 0x08, 0x08, 0x08, 0x04, 0x02, 0x00], // (
    [0x08, 0x04, 0x02, 0x02, 0x02, 0x04, 0x08, 0x00], // )
    [0x00, 0x04, 0x15, 0x0e, 0x15, 0x04, 0x00, 0x00], // *
    [0x00, 0x04, 0x04, 0x1f, 0x04, 0x04, 0x00, 0x00], // +
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x0c, 0x04, 0x08], // ,
    [0x00, 0x00, 0x00, 0x1f, 0x00, 0x00, 0x00, 0x00], // -
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x0c, 0x0c, 0x00], // .
    [0x00, 0x01, 0x02, 0x04, 0x08, 0x10, 0x00, 0x00], // /
    [0x0e, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0e, 0x00], // 0
    [0x04, 0x0c, 0x04, 0x04, 0x04, 0x04, 0x0e, 0x00], // 1
    [0x0e, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1f, 0x00], // 2
    [0x1f, 0x02, 0x04, 0x02, 0x01, 0x11, 0x0e, 0x00], // 3
    [0x02, 0x06, 0x0a, 0x12, 0x1f, 0x02, 0x02, 0x00], // 4
    [0x1f, 0x10, 0x1e, 0x01, 0x01, 0x11, 0x0e, 0x00], // 5
    [0x06, 0x08, 0x10, 0x1e, 0x11, 0x11, 0x0e, 0x00], // 6
    [0x1f, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08, 0x00], // 7
    [0x0e, 0x11, 0x11, 0x0e, 0x11, 0x11, 0x0e, 0x00], // 8
    [0x0e, 0x11, 0x11, 0x0f, 0x01, 0x02, 0x0c, 0x00], // 9
    [0x00, 0x0c, 0x0c, 0x00, 0x0c, 0x0c, 0x00, 0x00], // :
    [0x00, 0x0c, 0x0c, 0x00, 0x0c, 0x04, 0x08, 0x00], // ;
    [0x02, 0x04, 0x08, 0x10, 0x08, 0x04, 0x02, 0x00], // <
    [0x00, 0x00, 0x1f, 0x00, 0x1f, 0x00, 0x00, 0x00], // =
    [0x08, 0x04, 0x02, 0x01, 0x02, 0x04, 0x08, 0x00], // >
    [0x0e, 0x11, 0x01, 0x02, 0x04, 0x00, 0x04, 0x00], // ?
    [0x0e, 0x11, 0x01, 0x0d, 0x15, 0x15, 0x0e, 0x00], // @
    [0x0e, 0x11, 0x11, 0x11, 0x1f, 0x11, 0x11, 0x00], // A
    [0x1e, 0x11, 0x11, 0x1e, 0x11, 0x11, 0x1e, 0x00], // B
    [0x0e, 0x11, 0x10, 0x10, 0x10, 0x11, 0x0e, 0x00], // C
    [0x1c, 0x12, 0x11, 0x11, 0x11, 0x12, 0x1c, 0x00], // D
    [0x1f, 0x10, 0x10, 0x1e, 0x10, 0x10, 0x1f, 0x00], // E
    [0x1f, 0x10, 0x10, 0x1e, 0x10, 0x10, 0x10, 0x00], // F
    [0x0e, 0x11, 0x10, 0x17, 0x11, 0x11, 0x0f, 0x00], // G
    [0x11, 0x11, 0x11, 0x1f, 0x11, 0x11, 0x11, 0x00], // H
    [0x0e, 0x04, 0x04, 0x04, 0x04, 0x04, 0x0e, 0x00], // I
    [0x07, 0x02, 0x02, 0x02, 0x02, 0x12, 0x0c, 0x00], // J
    [0x11, 0x12, 0x14, 0x18, 0x14, 0x12, 0x11, 0x00], // K
    [0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x1f, 0x00], // L
    [0x11, 0x1b, 0x15, 0x15, 0x11, 0x11, 0x11, 0x00], // M
    [0x11, 0x11, 0x19, 0x15, 0x13, 0x11, 0x11, 0x00], // N
    [0x0e, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0e, 0x00], // O
    [0x1e, 0x11, 0x11, 0x1e, 0x10, 0x10, 0x10, 0x00], // P
    [0x0e, 0x11, 0x11, 0x11, 0x15, 0x12, 0x0d, 0x00], // Q
    [0x1e, 0x11, 0x11, 0x1e, 0x14, 0x12, 0x11, 0x00], // R
    [0x0f, 0x10, 0x10, 0x0e, 0x01, 0x01, 0x1e, 0x00], // S
    [0x1f, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x00], // T
    [0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0e, 0x00], // U
    [0x11, 0x11, 0x11, 0x11, 0x11, 0x0a, 0x04, 0x00], // V
    [0x11, 0x11, 0x11, 0x15, 0x15, 0x15, 0x0a, 0x00], // W
    [0x11, 0x11, 0x0a, 0x04, 0x0a, 0x11, 0x11, 0x00], // X
    [0x11, 0x11, 0x11, 0x0a, 0x04, 0x04, 0x04, 0x00], // Y
    [0x1f, 0x01, 0x02, 0x04, 0x08, 0x10, 0x1f, 0x00], // Z
    [0x0e, 0x08, 0x08, 0x08, 0x08, 0x08, 0x0e, 0x00], // [
    [0x00, 0x10, 0x08, 0x04, 0x02, 0x01, 0x00, 0x00], // backslash
    [0x0e, 0x02, 0x02, 0x02, 0x02, 0x02, 0x0e, 0x00], // ]
    [0x04, 0x0a, 0x11, 0x00, 0x00, 0x00, 0x00, 0x00], // ^
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x1f, 0x00], // _
    [0x08, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // `
    [0x00, 0x00, 0x0e, 0x01, 0x0f, 0x11, 0x0f, 0x00], // a
    [0x10, 0x10, 0x16, 0x19, 0x11, 0x11, 0x1e, 0x00], // b
    [0x00, 0x00, 0x0e, 0x10, 0x10, 0x11, 0x0e, 0x00], // c
    [0x01, 0x01, 0x0d, 0x13, 0x11, 0x11, 0x0f, 0x00], // d
    [0x00, 0x00, 0x0e, 0x11, 0x1f, 0x10, 0x0e, 0x00], // e
    [0x06, 0x09, 0x08, 0x1c, 0x08, 0x08, 0x08, 0x00], // f
    [0x00, 0x00, 0x0f, 0x11, 0x11, 0x0f, 0x01, 0x0e], // g
    [0x10, 0x10, 0x16, 0x19, 0x11, 0x11, 0x11, 0x00], // h
    [0x04, 0x00, 0x0c, 0x04, 0x04, 0x04, 0x0e, 0x00], // i
    [0x02, 0x00, 0x06, 0x02, 0x02, 0x02, 0x12, 0x0c], // j
    [0x10, 0x10, 0x12, 0x14, 0x18, 0x14, 0x12, 0x00], // k
    [0x0c, 0x04, 0x04, 0x04, 0x04, 0x04, 0x0e, 0x00], // l
    [0x00, 0x00, 0x1a, 0x15, 0x15, 0x11, 0x11, 0x00], // m
    [0x00, 0x00, 0x16, 0x19, 0x11, 0x11, 0x11, 0x00], // n
    [0x00, 0x00, 0x0e, 0x11, 0x11, 0x11, 0x0e, 0x00], // o
    [0x00, 0x00, 0x1e, 0x11, 0x11, 0x1e, 0x10, 0x10], // p
    [0x00, 0x00, 0x0f, 0x11, 0x11, 0x0f, 0x01, 0x01], // q
    [0x00, 0x00, 0x16, 0x19, 0x10, 0x10, 0x10, 0x00], // r
    [0x00, 0x00, 0x0f, 0x10, 0x0e, 0x01, 0x1e, 0x00], // s
    [0x08, 0x08, 0x1c, 0x08, 0x08, 0x09, 0x06, 0x00], // t
    [0x00, 0x00, 0x11, 0x11, 0x11, 0x13, 0x0d, 0x00], // u
    [0x00, 0x00, 0x11, 0x11, 0x11, 0x0a, 0x04, 0x00], // v
    [0x00, 0x00, 0x11, 0x11, 0x15, 0x15, 0x0a, 0x00], // w
    [0x00, 0x00, 0x11, 0x0a, 0x04, 0x0a, 0x11, 0x00], // x
    [0x00, 0x00, 0x11, 0x11, 0x11, 0x0f, 0x01, 0x0e], // y
    [0x00, 0x00, 0x1f, 0x02, 0x04, 0x08, 0x1f, 0x00], // z
    [0x02, 0x04, 0x04, 0x08, 0x04, 0x04, 0x02, 0x00], // {
    [0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x00], // |
    [0x08, 0x04, 0x04, 0x02, 0x04, 0x04, 0x08, 0x00], // }
    [0x00, 0x00, 0x08, 0x15, 0x02, 0x00, 0x00, 0x00], // ~
];

/// `text` with every character the font lacks as `?`.
fn printable(text: &str) -> String {
    text.chars()
        .map(|c| if (' '..='~').contains(&c) { c } else { '?' })
        .collect()
}

/// `text` ([`printable`]) in lines of at most `max` characters, broken at
/// spaces where it can be; a word longer than a line is cut.
fn wrap(text: &str, max: usize) -> Vec<String> {
    let max = max.max(1);
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in printable(text).split(' ') {
        let mut word = word;
        loop {
            let room = if line.is_empty() {
                max
            } else {
                max.saturating_sub(line.len() + 1)
            };
            if word.len() <= room {
                if !line.is_empty() {
                    line.push(' ');
                }
                line.push_str(word);
                break;
            }
            if line.is_empty() {
                lines.push(word[..max].to_owned());
                word = &word[max..];
            } else {
                lines.push(std::mem::take(&mut line));
            }
        }
    }
    lines.push(line);
    lines
}

/// A line's characters as the shader takes them, four a word, the first in
/// the low byte (at most [`LINE`]), and how many.
fn pack(line: &str) -> ([u32; LINE / 4], u32) {
    let mut words = [0; LINE / 4];
    let text = printable(line);
    let bytes = &text.as_bytes()[..text.len().min(LINE)];
    for (i, &b) in bytes.iter().enumerate() {
        words[i / 4] |= u32::from(b) << (i % 4 * 8);
    }
    (words, bytes.len() as u32)
}

/// The bitmap font as the atlas holds a face ([`atlas`]): each font pixel
/// `scale` pixels, in cells of 6x10 font pixels (a column and a row each
/// side of the 5x8 glyph but the right).
fn bitmap(scale: u32) -> (Vec<u8>, [u32; 2]) {
    let [w, h] = [6 * scale, 10 * scale];
    let mut glyphs = Vec::with_capacity(GLYPHS.len() * (w * h) as usize);
    for glyph in &GLYPHS {
        for y in 0..h {
            for x in 0..w {
                let (x, y) = (x / scale, y / scale);
                let on = (1..=8).contains(&y) && x < 5 && glyph[y as usize - 1] >> (4 - x) & 1 != 0;
                glyphs.push(if on { 255 } else { 0 });
            }
        }
    }
    (glyphs, [w, h])
}

/// Characters 32 to 126 in Consolas at `em` pixels an em, anti-aliased in
/// grey, as the atlas holds a face ([`atlas`]), and their cell: Consolas's
/// advance by its height. `None` if Windows has no Consolas.
fn consolas(em: u32) -> Option<(Vec<u8>, [u32; 2])> {
    use windows::Win32::Foundation::COLORREF;
    use windows::Win32::Graphics::Gdi::*;
    use windows::core::{PCWSTR, w};
    // SAFETY: a memory device context, font and DIB section of our own,
    // released before returning; the DIB's bits are read once GDI has
    // drawn (GdiFlush) and while the section lives.
    unsafe {
        let dc = CreateCompatibleDC(None);
        if dc.is_invalid() {
            return None;
        }
        let font = CreateFontW(
            -(em as i32),
            0,
            0,
            0,
            FW_NORMAL.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_TT_ONLY_PRECIS,
            CLIP_DEFAULT_PRECIS,
            ANTIALIASED_QUALITY,
            u32::from(FIXED_PITCH.0 | FF_MODERN.0),
            w!("Consolas"),
        );
        let old_font = SelectObject(dc, font.into());
        let mut name = [0u16; 32];
        let named = GetTextFaceW(dc, Some(&mut name)).max(0) as usize;
        let mut metrics = TEXTMETRICW::default();
        // GDI gives another face when it has no Consolas.
        let found = String::from_utf16_lossy(&name[..named.min(name.len())])
            .trim_end_matches('\0')
            .eq_ignore_ascii_case("Consolas")
            && GetTextMetricsW(dc, &mut metrics).as_bool();
        let [w, h] = [
            metrics.tmAveCharWidth.max(0) as u32,
            metrics.tmHeight.max(0) as u32,
        ];
        let wide = w * GLYPHS.len() as u32;
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: wide as i32,
                // Top row first.
                biHeight: -(h as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let section = (found && w > 0 && h > 0)
            .then(|| CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0).ok())
            .flatten();
        let mut made = None;
        if let Some(section) = section {
            let old_bitmap = SelectObject(dc, section.into());
            SetTextColor(dc, COLORREF(0x00ff_ffff));
            SetBkColor(dc, COLORREF(0));
            SetBkMode(dc, OPAQUE);
            let text: Vec<u16> = (32..=126).collect();
            let advances = vec![w as i32; text.len()];
            let drawn = ExtTextOutW(
                dc,
                0,
                0,
                ETO_OPTIONS(0),
                None,
                PCWSTR(text.as_ptr()),
                text.len() as u32,
                Some(advances.as_ptr()),
            );
            let _ = GdiFlush();
            if drawn.as_bool() {
                // White on black, grey anti-aliasing: any channel is the
                // coverage; green, of blue, green, red, unused.
                let pixels = std::slice::from_raw_parts(bits.cast::<u8>(), (wide * h * 4) as usize);
                let mut glyphs = Vec::with_capacity((wide * h) as usize);
                for glyph in 0..GLYPHS.len() as u32 {
                    for y in 0..h {
                        for x in 0..w {
                            glyphs.push(pixels[((y * wide + glyph * w + x) * 4 + 1) as usize]);
                        }
                    }
                }
                made = Some((glyphs, [w, h]));
            }
            SelectObject(dc, old_bitmap);
            let _ = DeleteObject(section.into());
        }
        SelectObject(dc, old_font);
        let _ = DeleteObject(font.into());
        let _ = DeleteDC(dc);
        made
    }
}

/// The atlas for a window `height` pixels high, and its faces (big, main,
/// small): Consolas at [`EMS`] pixels an em for 1080, or the bitmap font if
/// Windows has none. A face is characters 32 to 126, a byte of coverage a
/// pixel, glyph after glyph, each row after row (as the shader reads it).
fn atlas(height: u32) -> (Vec<u8>, [Face; 3]) {
    let ems = EMS.map(|em| ((em * height + 540) / 1080).max(8));
    let glyphs = match ems.map(consolas) {
        [Some(big), Some(main), Some(small)] => [big, main, small],
        _ => {
            tracing::warn!("no Consolas: the overlay draws with its bitmap font");
            let scale = (height / 1080).max(1);
            [bitmap(2 * scale), bitmap(scale), bitmap(scale)]
        }
    };
    let mut bytes = Vec::new();
    let faces = glyphs.map(|(glyphs, cell)| {
        let base = bytes.len() as u32;
        bytes.extend(glyphs);
        Face { cell, base }
    });
    (bytes, faces)
}

/// A row of the table.
#[derive(Debug, Clone, PartialEq)]
enum Row {
    /// The overlay's name, and the console the title sees.
    Title(&'static str),
    /// Two figures at the row's ends in the big face, in a colour.
    Big(String, String, u32),
    /// A label and its value against the right edge; with a share (0 to 1),
    /// a bar of it between them.
    Item(&'static str, String, Option<f64>),
    /// A label and its value in the small face.
    Detail(&'static str, String),
    /// A thin line between groups.
    Rule,
    /// Grey text in the small face, in lines that fit the table.
    Note(String),
}

/// What the panel draws, in order, in the window's pixels.
#[derive(Debug, Clone, PartialEq)]
enum Shape {
    /// A rectangle (left, top, right, bottom) filled with a colour.
    Fill([u32; 4], u32),
    /// Text: its cells' top left, its face, the characters, the colour.
    Text {
        at: [u32; 2],
        face: usize,
        text: String,
        colour: u32,
    },
}

/// The headline's colour for `fps` against the 15 to 20 frames a second
/// aimed at: green from 15, amber from 10, red below.
fn fps_colour(fps: f64) -> u32 {
    if fps >= 15.0 {
        GREEN
    } else if fps >= 10.0 {
        AMBER
    } else {
        RED
    }
}

/// What the table shows besides the frame line, read when its rows are
/// made: the console the title sees, shaders found in the cache and
/// translated, host pipelines loaded from the library and made, those
/// being made, the card's memory (`Host::video_memory`).
struct Counts {
    console: &'static str,
    shaders: (u64, u64),
    pipelines: [u64; 2],
    making: u64,
    memory: Option<[u64; 4]>,
}

impl Counts {
    fn now(host: &Host) -> Counts {
        let console = crate::console::current().name();
        Counts {
            console: console.strip_prefix("Xbox ").unwrap_or(console),
            shaders: crate::shader_cache::counts(),
            pipelines: crate::pipeline::library_pipelines(),
            making: crate::pipeline::making(),
            memory: host.video_memory(),
        }
    }
}

/// The table's rows: `line`'s figures and `counts`; with `details` (the
/// live switches set), the diagnostics block.
fn rows(line: &FrameLine, counts: &Counts, details: Option<&[(&'static str, String)]>) -> Vec<Row> {
    const GB: f64 = (1u64 << 30) as f64;
    let gb = |bytes: u64| bytes as f64 / GB;
    let ms = |ms: f64| format!("{ms:.1} ms");
    // Each against the whole frame: they overlap.
    let share = |part: f64| Some(if line.ms > 0.0 { part / line.ms } else { 0.0 });
    let mut rows = vec![
        Row::Title(counts.console),
        // Whole milliseconds: a clear gap after the frames a second.
        Row::Big(
            format!("{:.1} fps", line.fps),
            format!("{:.0} ms", line.ms),
            fps_colour(line.fps),
        ),
        Row::Rule,
        Row::Item("CPU prep", ms(line.cpu), share(line.cpu)),
        Row::Item("GPU busy", ms(line.busy), share(line.busy)),
        Row::Item("Copy-back", ms(line.back), share(line.back)),
        Row::Rule,
        Row::Item("Draws", format!("{:.0}", line.draws), None),
        Row::Item("Dropped", format!("{:.0}", line.dropped), None),
        Row::Rule,
        Row::Item(
            "VRAM",
            match counts.memory {
                Some([budget, used, ..]) => format!("{:.2} / {:.2} GB", gb(used), gb(budget)),
                None => "unknown".to_owned(),
            },
            None,
        ),
    ];
    if let Some(live) = details {
        let (cached, translated) = counts.shaders;
        let [loaded, made] = counts.pipelines;
        let [slots, refused] = line.dynamic;
        rows.extend([
            Row::Rule,
            Row::Detail("GPU wait", ms(line.wait)),
            Row::Detail("Dispatches", format!("{:.0}", line.dispatches)),
            Row::Detail("Shaders", format!("{cached} + {translated} new")),
            Row::Detail("Pipelines", format!("{loaded} + {made} new")),
            Row::Detail("Compiling", counts.making.to_string()),
        ]);
        if let Some([.., shared]) = counts.memory {
            rows.push(Row::Detail(
                "Shared memory",
                format!("{:.2} GB", gb(shared)),
            ));
        }
        rows.push(Row::Detail(
            "Dynamic slots",
            if refused >= 0.5 {
                format!("{slots:.0}, {refused:.0} refused")
            } else {
                format!("{slots:.0}")
            },
        ));
        rows.extend(
            live.iter()
                .map(|(name, value)| Row::Detail(name, value.clone())),
        );
    }
    rows
}

/// The shapes of `rows` drawn with `faces`: the panel, then each row's.
/// The rows are as wide as the widest needs, at least [`NARROWEST`]
/// characters of the main face, at most `room` pixels; the panel's
/// measures follow the main face.
fn layout(rows: &[Row], faces: &[Face; 3], room: u32) -> Vec<Shape> {
    let [cw, ch] = faces[MAIN].cell;
    let bars = || {
        rows.iter().filter_map(|row| match row {
            Row::Item(label, value, Some(_)) => Some((label.len(), value.len())),
            _ => None,
        })
    };
    let bar_label = bars().map(|(label, _)| label).max().unwrap_or(0);
    let bar_value = bars()
        .map(|(_, value)| value)
        .max()
        .unwrap_or(0)
        .max(BAR_VALUE);
    let chars = |face: usize, n: usize| n as u32 * faces[face].cell[0];
    let needs = |row: &Row| match row {
        Row::Title(console) => chars(MAIN, "VIctory-reloaded".len() + 2 + console.len()),
        Row::Big(left, right, _) => chars(BIG, left.len() + 1 + right.len()),
        Row::Item(label, value, None) => chars(MAIN, label.len() + 2 + value.len()),
        Row::Item(_, _, Some(_)) => chars(MAIN, bar_label + 1 + BAR_CHARS + 1 + bar_value),
        Row::Detail(label, value) => chars(SMALL, label.len() + 2 + value.len()),
        Row::Rule | Row::Note(_) => 0,
    };
    let width = rows
        .iter()
        .map(needs)
        .max()
        .unwrap_or(0)
        .max(chars(MAIN, NARROWEST))
        .min(room)
        .max(cw);
    let (margin, pad) = (ch / 2, [cw, ch / 3]);
    let left = margin + pad[0];
    let right = left + width;
    // Bars run from a character after the label column to one before the
    // value column.
    let from = left + chars(MAIN, bar_label + 1);
    let to = right.saturating_sub(chars(MAIN, bar_value + 1));
    let mut shapes = vec![Shape::Fill([0; 4], BACKGROUND)];
    let mut y = margin + pad[1];
    // A label at the left and a value against the right edge, in `face`
    // and its row's height; the value is cut to leave a space after the
    // label, which is cut to the row.
    let mut pair =
        |y: &mut u32, face: usize, (label, value): (&str, &str), [ink, value_ink]: [u32; 2]| {
            let room = (width / faces[face].cell[0]) as usize;
            let (label, value) = (printable(label), printable(value));
            let label = &label[..label.len().min(room)];
            let value = &value[..value.len().min(room.saturating_sub(label.len() + 1))];
            let text = |x, text: &str, colour| Shape::Text {
                at: [x, *y],
                face,
                text: text.to_owned(),
                colour,
            };
            shapes.push(text(left, label, ink));
            if !value.is_empty() {
                shapes.push(text(right - chars(face, value.len()), value, value_ink));
            }
            *y += faces[face].cell[1];
        };
    let mut fills = Vec::new();
    let mut notes = Vec::new();
    for row in rows {
        match row {
            Row::Title(console) => {
                pair(&mut y, MAIN, ("VIctory-reloaded", console), [VALUE, LABEL])
            }
            Row::Big(l, r, colour) => pair(&mut y, BIG, (l.as_str(), r.as_str()), [*colour; 2]),
            Row::Item(label, value, bar) => {
                if let Some(share) = bar
                    && to > from
                {
                    let thick = (ch / 4).max(2);
                    let top = y + (ch - thick) / 2;
                    fills.push(Shape::Fill([from, top, to, top + thick], TRACK));
                    let filled = (f64::from(to - from) * share.clamp(0.0, 1.0)).round() as u32;
                    if filled > 0 {
                        fills.push(Shape::Fill([from, top, from + filled, top + thick], BAR));
                    }
                }
                pair(&mut y, MAIN, (*label, value.as_str()), [LABEL, VALUE]);
            }
            Row::Detail(label, value) => {
                pair(&mut y, SMALL, (*label, value.as_str()), [LABEL, DETAIL]);
            }
            Row::Rule => {
                let top = y + ch / 4;
                fills.push(Shape::Fill(
                    [left, top, right, top + (ch / 16).max(1)],
                    RULE,
                ));
                y += ch / 2;
            }
            Row::Note(note) => {
                let room = (width / faces[SMALL].cell[0]) as usize;
                for line in wrap(note, room) {
                    notes.push(Shape::Text {
                        at: [left, y],
                        face: SMALL,
                        text: line,
                        colour: LABEL,
                    });
                    y += faces[SMALL].cell[1];
                }
            }
        }
    }
    shapes[0] = Shape::Fill([margin, margin, right + pad[0], y + pad[1]], BACKGROUND);
    shapes.splice(1..1, fills);
    shapes.extend(notes);
    shapes
}

/// The shaders' constants for `shape` drawn with `faces` in a window of
/// `size`.
fn constants(shape: &Shape, faces: &[Face; 3], size: [u32; 2]) -> Constants {
    let target = size.map(|s| s as f32);
    match shape {
        Shape::Fill(rect, colour) => Constants {
            rect: rect.map(|v| v as f32),
            target,
            style: [*colour, 0],
            ..Default::default()
        },
        Shape::Text {
            at: [x, y],
            face,
            text,
            colour,
        } => {
            let (words, count) = pack(text);
            let Face { cell: [w, h], base } = faces[*face];
            Constants {
                rect: [*x as f32, *y as f32, (x + count * w) as f32, (y + h) as f32],
                target,
                style: [*colour, w | h << 8],
                head: [count, base, 0, 0],
                words,
            }
        }
    }
}

/// The panel's draws in a window of `size` with `faces`: `line`'s table
/// (with the counts as they are now), if any, and `note` under it.
fn draws(
    host: &Host,
    line: Option<&FrameLine>,
    size: [u32; 2],
    faces: &[Face; 3],
    note: Option<&str>,
) -> Vec<Constants> {
    let mut table = match line {
        Some(line) => {
            #[cfg(feature = "diagnostics")]
            let live = crate::live::active();
            #[cfg(feature = "diagnostics")]
            let details = Some(&live[..]);
            #[cfg(not(feature = "diagnostics"))]
            let details = None;
            rows(line, &Counts::now(host), details)
        }
        None => Vec::new(),
    };
    if let Some(note) = note {
        if !table.is_empty() {
            table.push(Row::Rule);
        }
        table.push(Row::Note(note.to_owned()));
    }
    // The window's width less the panel's margin and padding each side.
    let [cw, ch] = faces[MAIN].cell;
    let room = size[0].saturating_sub(2 * (ch / 2 + cw));
    layout(&table, faces, room)
        .iter()
        .map(|shape| constants(shape, faces, size))
        .collect()
}

/// Compiles `entry` of overlay.hlsl for `target`; the compiler's messages
/// if it failed.
fn compile(entry: PCSTR, target: PCSTR) -> Result<ID3DBlob, String> {
    let (mut code, mut errors): (Option<ID3DBlob>, Option<ID3DBlob>) = (None, None);
    // SAFETY: the source and names live through the call; the out pointers
    // are ours.
    let made = unsafe {
        D3DCompile(
            SOURCE.as_ptr().cast(),
            SOURCE.len(),
            s!("overlay.hlsl"),
            None,
            None,
            entry,
            target,
            0,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    match (made, code) {
        (Ok(()), Some(code)) => Ok(code),
        (made, _) => Err(errors.map_or_else(
            || {
                made.err()
                    .map_or_else(|| "no code".to_owned(), |e| e.to_string())
            },
            |e| {
                String::from_utf8_lossy(bytes(&e))
                    .trim_end_matches('\0')
                    .trim()
                    .to_owned()
            },
        )),
    }
}

/// A blob's bytes.
fn bytes(blob: &ID3DBlob) -> &[u8] {
    // SAFETY: the blob's own bytes, alive while it is.
    unsafe {
        std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize())
    }
}

/// Pipelines that draw rectangles under `root` with a vertex and a pixel
/// shader, blending by alpha or not, made for a window buffer's format at
/// its first use (`None`: the host refused it).
struct Pipelines {
    root: ID3D12RootSignature,
    shaders: [ID3DBlob; 2],
    blend: bool,
    made: Vec<(DXGI_FORMAT, Option<ID3D12PipelineState>)>,
}

impl Pipelines {
    /// Compiles the shaders `entries` (vertex, pixel); the root signature is
    /// the vertex shader's.
    fn new(host: &Host, entries: [PCSTR; 2], blend: bool) -> Result<Pipelines, String> {
        let [vs, ps] = entries;
        let shaders = [compile(vs, s!("vs_5_1"))?, compile(ps, s!("ps_5_1"))?];
        // SAFETY: the shader's blob carries the root signature.
        let root = unsafe {
            host.device
                .CreateRootSignature::<ID3D12RootSignature>(0, bytes(&shaders[0]))
        }
        .map_err(|e| e.to_string())?;
        Ok(Pipelines {
            root,
            shaders,
            blend,
            made: Vec::new(),
        })
    }

    fn get(&mut self, host: &Host, format: DXGI_FORMAT) -> Option<ID3D12PipelineState> {
        if let Some((_, made)) = self.made.iter().find(|(f, _)| *f == format) {
            return made.clone();
        }
        let mut targets = D3D12_BLEND_DESC::default();
        targets.RenderTarget[0] = D3D12_RENDER_TARGET_BLEND_DESC {
            BlendEnable: self.blend.into(),
            LogicOpEnable: false.into(),
            SrcBlend: D3D12_BLEND_SRC_ALPHA,
            DestBlend: D3D12_BLEND_INV_SRC_ALPHA,
            BlendOp: D3D12_BLEND_OP_ADD,
            SrcBlendAlpha: D3D12_BLEND_ONE,
            DestBlendAlpha: D3D12_BLEND_INV_SRC_ALPHA,
            BlendOpAlpha: D3D12_BLEND_OP_ADD,
            LogicOp: D3D12_LOGIC_OP_NOOP,
            RenderTargetWriteMask: D3D12_COLOR_WRITE_ENABLE_ALL.0 as u8,
        };
        let mut formats = [DXGI_FORMAT_UNKNOWN; 8];
        formats[0] = format;
        let [vs, ps] = &self.shaders;
        // SAFETY: the description points at live data and never releases
        // its pointer.
        let made = unsafe {
            host.device
                .CreateGraphicsPipelineState(&D3D12_GRAPHICS_PIPELINE_STATE_DESC {
                    pRootSignature: std::mem::transmute_copy(&self.root),
                    VS: bytecode(bytes(vs)),
                    PS: bytecode(bytes(ps)),
                    BlendState: targets,
                    SampleMask: u32::MAX,
                    RasterizerState: D3D12_RASTERIZER_DESC {
                        FillMode: D3D12_FILL_MODE_SOLID,
                        CullMode: D3D12_CULL_MODE_NONE,
                        DepthClipEnable: true.into(),
                        ..Default::default()
                    },
                    PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
                    NumRenderTargets: 1,
                    RTVFormats: formats,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    ..Default::default()
                })
        };
        let made = made
            .inspect_err(|e| tracing::warn!(error = %e, format = format.0, "no overlay pipeline for this window format"))
            .ok();
        self.made.push((format, made.clone()));
        made
    }
}

/// The objects the overlay draws with, made at its first use.
pub(crate) struct Overlay {
    pipelines: Pipelines,
    /// The atlas ([`atlas`]) in upload memory, the window height it was
    /// drawn for, and its faces.
    font: Option<(ID3D12Resource, u32, [Face; 3])>,
    /// Atlases drawn for other heights: a list in flight may still read
    /// one.
    retired: Vec<ID3D12Resource>,
    /// The panel's draws as last made.
    drawn: Option<Drawn>,
    /// The view's objects, made at the first view (`None` inside: none
    /// could be made).
    #[cfg(feature = "diagnostics")]
    view: Option<Option<view::View>>,
}

// SAFETY: D3D12 objects are free-threaded; the presenter's lock guards the
// rest.
unsafe impl Send for Overlay {}

/// The panel's draws for a frame line (`None`: no table), a window size and
/// a note under the table.
struct Drawn {
    line: Option<FrameLine>,
    size: [u32; 2],
    note: Option<String>,
    draws: Vec<Constants>,
}

impl Overlay {
    fn new(host: &Host) -> Option<Overlay> {
        match Pipelines::new(host, [s!("corners"), s!("panel")], true) {
            Ok(pipelines) => {
                tracing::info!("overlay made");
                Some(Overlay {
                    pipelines,
                    font: None,
                    retired: Vec::new(),
                    drawn: None,
                    #[cfg(feature = "diagnostics")]
                    view: None,
                })
            }
            Err(e) => {
                tracing::warn!(error = %e, "no overlay");
                None
            }
        }
    }

    /// Records the panel at the top left of the window buffer `back` (in the
    /// render target state): `line`'s table, if any, and `note` under it.
    /// Its draws are made again when either changed, or the window's size;
    /// its atlas when the window's height did.
    fn panel(
        &mut self,
        host: &Host,
        list: &ID3D12GraphicsCommandList,
        back: &ID3D12Resource,
        line: Option<&FrameLine>,
        note: Option<&str>,
    ) {
        // SAFETY: a live resource.
        let d = unsafe { back.GetDesc() };
        let size = [d.Width as u32, d.Height];
        let Some(pipeline) = self.pipelines.get(host, d.Format) else {
            return;
        };
        if self
            .font
            .as_ref()
            .is_none_or(|(_, height, _)| *height != size[1])
        {
            let (bytes, faces) = atlas(size[1]);
            let Some((buffer, data)) = host.upload_buffer(bytes.len() as u64) else {
                return;
            };
            // SAFETY: the upload buffer's mapping holds as many bytes.
            unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), data, bytes.len()) };
            crate::host::name(&buffer, || "overlay font".to_owned());
            if let Some((old, ..)) = self.font.replace((buffer, size[1], faces)) {
                self.retired.push(old);
            }
        }
        let Some((font, _, faces)) = &self.font else {
            return;
        };
        let line = line.copied();
        let drawn = match self.drawn.take() {
            Some(drawn)
                if drawn.line == line && drawn.size == size && drawn.note.as_deref() == note =>
            {
                drawn
            }
            _ => Drawn {
                line,
                size,
                note: note.map(str::to_owned),
                draws: draws(host, line.as_ref(), size, faces, note),
            },
        };
        let drawn = self.drawn.insert(drawn);
        let words = (size_of::<Constants>() / 4) as u32;
        // SAFETY: an open list; our root signature and atlas, which live as
        // long as the overlay; a view of the window buffer from the host's
        // ring, made where it is recorded (the ring comes round).
        unsafe {
            list.SetGraphicsRootSignature(&self.pipelines.root);
            list.SetGraphicsRootShaderResourceView(FONT, font.GetGPUVirtualAddress());
            target(list, host.rtv(Some(back), None), size);
            list.SetPipelineState(&pipeline);
            for constants in &drawn.draws {
                list.SetGraphicsRoot32BitConstants(
                    DRAW,
                    words,
                    std::ptr::from_ref(constants).cast(),
                    0,
                );
                list.DrawInstanced(4, 1, 0, 0);
            }
        }
    }
}

/// Sets `rtv` as the only target, with a viewport and scissor of `size`,
/// for strips.
///
/// # Safety
/// `list` is open; `rtv` is a live view.
unsafe fn target(
    list: &ID3D12GraphicsCommandList,
    rtv: D3D12_CPU_DESCRIPTOR_HANDLE,
    size: [u32; 2],
) {
    // SAFETY: as the caller promises.
    unsafe {
        list.OMSetRenderTargets(1, Some(&rtv), false, None);
        list.RSSetViewports(&[D3D12_VIEWPORT {
            TopLeftX: 0.0,
            TopLeftY: 0.0,
            Width: size[0] as f32,
            Height: size[1] as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        }]);
        list.RSSetScissorRects(&[RECT {
            left: 0,
            top: 0,
            right: size[0] as i32,
            bottom: size[1] as i32,
        }]);
        list.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
    }
}

fn bytecode(code: &[u8]) -> D3D12_SHADER_BYTECODE {
    D3D12_SHADER_BYTECODE {
        pShaderBytecode: code.as_ptr().cast(),
        BytecodeLength: code.len(),
    }
}

/// At a present: records into `list`, after the frame's copy into the
/// window buffer `back` (in the copy destination state, where it is left),
/// the overlay of `line` if it is on. `made` holds its objects. (In
/// diagnostics builds [`view::record`] does this, with the control file's
/// asks.)
#[cfg(not(feature = "diagnostics"))]
pub(crate) fn record(
    made: &mut Option<Option<Overlay>>,
    host: &Host,
    list: &HostList,
    back: &ID3D12Resource,
    line: &FrameLine,
) {
    if !on() {
        return;
    }
    let Some(o) = made.get_or_insert_with(|| Overlay::new(host)) else {
        return;
    };
    let list = &list.list;
    let (copy, draw) = (
        D3D12_RESOURCE_STATE_COPY_DEST,
        D3D12_RESOURCE_STATE_RENDER_TARGET,
    );
    // SAFETY: an open list and the window's live buffer.
    unsafe { list.ResourceBarrier(&[crate::host::transition(back, copy, draw)]) };
    o.panel(host, list, back, Some(line), None);
    // SAFETY: as above.
    unsafe { list.ResourceBarrier(&[crate::host::transition(back, draw, copy)]) };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The headline is green from 15 frames a second, amber from 10, red
    /// below.
    #[test]
    fn the_headline_colour_follows_the_target() {
        assert_eq!(fps_colour(5.8), RED);
        assert_eq!(fps_colour(10.0), AMBER);
        assert_eq!(fps_colour(14.9), AMBER);
        assert_eq!(fps_colour(15.0), GREEN);
        assert_eq!(fps_colour(60.0), GREEN);
    }

    /// The glyphs fit five columns, the space is empty and every other
    /// character shows something.
    #[test]
    fn every_printable_character_has_a_glyph() {
        assert_eq!(GLYPHS.len(), usize::from(b'~' - b' ') + 1);
        for (i, glyph) in GLYPHS.iter().enumerate() {
            assert!(
                glyph.iter().all(|&row| row < 0x20),
                "glyph {i} is wider than 5 columns"
            );
            assert_eq!(glyph.iter().all(|&row| row == 0), i == 0, "glyph {i}");
        }
    }

    /// A glyph's pixel as overlay.hlsl finds it in a face: the face's base,
    /// the glyph's cells before it, the rows above, the column.
    fn coverage(bytes: &[u8], face: Face, c: u8, [x, y]: [u32; 2]) -> u8 {
        let [w, h] = face.cell;
        bytes[(face.base + (u32::from(c - 32) * h + y) * w + x) as usize]
    }

    /// The bitmap font's faces are where the shader reads them: the capital
    /// A in its cell, a row and a column of space around, twice as big at
    /// scale 2.
    #[test]
    fn the_bitmap_font_is_where_the_shader_reads_it() {
        let (bytes, cell) = bitmap(1);
        assert_eq!(cell, [6, 10]);
        assert_eq!(bytes.len(), GLYPHS.len() * 60);
        let face = Face { cell, base: 0 };
        let a: Vec<String> = (0..10)
            .map(|y| {
                (0..6)
                    .map(|x| match coverage(&bytes, face, b'A', [x, y]) {
                        255 => '#',
                        _ => '.',
                    })
                    .collect()
            })
            .collect();
        assert_eq!(
            a,
            [
                "......", ".###..", "#...#.", "#...#.", "#...#.", "#####.", "#...#.", "#...#.",
                "......", "......"
            ]
        );
        let (twice, cell) = bitmap(2);
        let face2 = Face { cell, base: 0 };
        assert_eq!(cell, [12, 20]);
        for (x, y) in [(1, 1), (0, 2), (4, 5)] {
            let one = coverage(&bytes, face, b'A', [x, y]);
            assert_eq!(coverage(&twice, face2, b'A', [2 * x + 1, 2 * y + 1]), one);
        }
    }

    /// GDI draws Consolas into cells taller than wide, a space as nothing,
    /// other characters with anti-aliased edges; twice the size, about twice
    /// the cell. (Skipped where Windows has no Consolas.)
    #[test]
    fn consolas_is_drawn_anti_aliased() {
        let (Some((bytes, cell)), Some((_, twice))) = (consolas(14), consolas(28)) else {
            eprintln!("no Consolas here");
            return;
        };
        let [w, h] = cell;
        assert!(w >= 6 && h > w && h <= 20, "{cell:?}");
        assert_eq!(bytes.len(), GLYPHS.len() * (w * h) as usize);
        let glyph = |c: u8| {
            let at = (u32::from(c - 32) * w * h) as usize;
            &bytes[at..at + (w * h) as usize]
        };
        assert!(glyph(b' ').iter().all(|&b| b == 0));
        for c in *b"#0g~" {
            assert!(glyph(c).iter().any(|&b| b > 200), "{}", c as char);
        }
        assert!(
            glyph(b'0').iter().any(|&b| b > 0 && b < 255),
            "anti-aliased"
        );
        assert!(
            twice[1].abs_diff(2 * h) <= 2 && twice[0].abs_diff(2 * w) <= 2,
            "{twice:?}"
        );
    }

    /// The atlas lays its faces one after the other, the big one the
    /// tallest.
    #[test]
    fn the_atlas_holds_three_faces() {
        let (bytes, faces) = atlas(1080);
        let size = |f: Face| GLYPHS.len() as u32 * f.cell[0] * f.cell[1];
        assert_eq!(faces[BIG].base, 0);
        assert_eq!(faces[MAIN].base, size(faces[BIG]));
        assert_eq!(faces[SMALL].base, faces[MAIN].base + size(faces[MAIN]));
        assert_eq!(bytes.len() as u32, faces[SMALL].base + size(faces[SMALL]));
        assert!(faces[BIG].cell[1] > faces[MAIN].cell[1]);
        assert!(faces[MAIN].cell[1] >= faces[SMALL].cell[1]);
    }

    #[test]
    fn lines_pack_four_characters_a_word() {
        let (words, count) = pack("AB\u{e9}~x");
        assert_eq!(count, 5);
        assert_eq!(words[0], u32::from_le_bytes(*b"AB?~"));
        assert_eq!(words[1], u32::from(b'x'));
        let (_, count) = pack(&"m".repeat(LINE + 10));
        assert_eq!(count as usize, LINE);
    }

    #[test]
    fn long_text_wraps_at_spaces_and_cuts_long_words() {
        assert_eq!(
            wrap("live switches: A=1 B=2", 14),
            ["live switches:", "A=1 B=2"]
        );
        assert_eq!(wrap("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        assert_eq!(wrap("short", 40), ["short"]);
        assert_eq!(wrap("", 40), [""]);
        assert!(
            wrap("frame 12: 3.0 ms and some more words here", 10)
                .iter()
                .all(|l| l.len() <= 10)
        );
    }

    /// The constants are the 44 words every root signature of overlay.hlsl
    /// gives them.
    #[test]
    fn the_constants_are_the_root_constants() {
        assert_eq!(size_of::<Constants>(), 44 * 4);
        let signatures = SOURCE.matches("RootConstants(").count();
        assert!(signatures > 0);
        assert_eq!(
            SOURCE
                .matches("RootConstants(num32BitConstants = 44, b0)")
                .count(),
            signatures
        );
    }

    /// The panel is drawn on the card over what the window buffer holds, at
    /// the top left, and nothing else is: in a small window and at 1080p,
    /// where each face has a size of its own.
    #[cfg(feature = "diagnostics")]
    #[test]
    fn the_panel_is_drawn_on_the_card() {
        let Some(host) = crate::host::host() else {
            eprintln!("skipping the overlay's draw: no D3D12 device");
            return;
        };
        // A pixel in the panel's padding, left of its text.
        drawn_on_the_card(host, [256, 128], [7, 7]);
        drawn_on_the_card(host, [1920, 1080], [10, 10]);
    }

    #[cfg(feature = "diagnostics")]
    fn drawn_on_the_card(host: &Host, [width, height]: [u32; 2], [x, y]: [usize; 2]) {
        let mut back: Option<ID3D12Resource> = None;
        // SAFETY: plain creation; the out pointer is ours.
        unsafe {
            host.device
                .CreateCommittedResource(
                    &D3D12_HEAP_PROPERTIES {
                        Type: D3D12_HEAP_TYPE_DEFAULT,
                        ..Default::default()
                    },
                    D3D12_HEAP_FLAG_NONE,
                    &D3D12_RESOURCE_DESC {
                        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                        Width: u64::from(width),
                        Height: height,
                        DepthOrArraySize: 1,
                        MipLevels: 1,
                        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                        SampleDesc: DXGI_SAMPLE_DESC {
                            Count: 1,
                            Quality: 0,
                        },
                        Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
                        ..Default::default()
                    },
                    D3D12_RESOURCE_STATE_RENDER_TARGET,
                    None,
                    &mut back,
                )
                .unwrap();
        }
        let back = back.unwrap();
        let list = host.new_list().unwrap();
        let cmds = &list.list;
        let (target, copy, read) = (
            D3D12_RESOURCE_STATE_RENDER_TARGET,
            D3D12_RESOURCE_STATE_COPY_DEST,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
        );
        // SAFETY: an open list and a live texture.
        unsafe {
            cmds.ClearRenderTargetView(host.rtv(Some(&back), None), &[1.0; 4], None);
            cmds.ResourceBarrier(&[crate::host::transition(&back, target, copy)]);
        }
        let line = FrameLine {
            fps: 30.0,
            ms: 33.3,
            ..Default::default()
        };
        let at = crate::control::AtPresent {
            overlay: true,
            view: None,
            shot: None,
        };
        let mut made = None;
        assert!(view::record(&mut made, host, &list, &back, &line, at).is_none());
        assert!(matches!(made, Some(Some(_))), "the overlay was made");
        // SAFETY: as above.
        unsafe { cmds.ResourceBarrier(&[crate::host::transition(&back, copy, read)]) };
        let (buffer, footprint) = crate::capture::read_back(&host.device, cmds, &back).unwrap();
        list.close();
        host.execute(&[&list]);
        let mut data: *mut std::ffi::c_void = std::ptr::null_mut();
        // SAFETY: a readback buffer of ours.
        unsafe { buffer.Map(0, None, Some(&mut data)).unwrap() };
        // SAFETY: the list ran; the mapping spans the footprint's rows.
        let pixel = |x: usize, y: usize| unsafe {
            let at = data
                .cast::<u8>()
                .add(y * footprint.Footprint.RowPitch as usize + x * 4);
            std::slice::from_raw_parts(at, 4).to_vec()
        };
        let (panel, away) = (pixel(x, y), pixel(width as usize - 6, height as usize - 8));
        // SAFETY: as above.
        unsafe { buffer.Unmap(0, None) };
        assert!(
            panel[..3].iter().all(|&c| c < 80),
            "{width}x{height}: the panel is dark: {panel:?}"
        );
        assert_eq!(
            away, [255; 4],
            "{width}x{height}: away from it the buffer is as it was"
        );
    }

    /// The shaders compile, the view's too.
    #[test]
    fn the_shaders_compile() {
        for (entry, target) in [
            (s!("corners"), s!("vs_5_1")),
            (s!("panel"), s!("ps_5_1")),
            (s!("view_corners"), s!("vs_5_1")),
            (s!("view"), s!("ps_5_1")),
            (s!("reset"), s!("cs_5_1")),
            (s!("reduce"), s!("cs_5_1")),
        ] {
            if let Err(e) = compile(entry, target) {
                panic!("{e}");
            }
        }
    }

    /// Faces the size of Consolas's at 24, 14 and 12 pixels an em.
    const FACES: [Face; 3] = [
        Face {
            cell: [13, 28],
            base: 0,
        },
        Face {
            cell: [8, 17],
            base: 0,
        },
        Face {
            cell: [7, 15],
            base: 0,
        },
    ];

    fn sample() -> (FrameLine, Counts) {
        let gb = |gb: f64| (gb * (1u64 << 30) as f64) as u64;
        let line = FrameLine {
            fps: 5.7,
            ms: 176.0,
            cpu: 102.6,
            busy: 48.3,
            wait: 0.4,
            back: 56.8,
            draws: 5030.4,
            dispatches: 4783.0,
            dropped: 340.0,
            dynamic: [61234.4, 0.2],
        };
        let counts = Counts {
            console: "Series X",
            shaders: (2319, 4),
            pipelines: [1977, 596],
            making: 0,
            memory: Some([gb(22.82), gb(14.99), 0, gb(5.5)]),
        };
        (line, counts)
    }

    fn items(rows: &[Row]) -> Vec<(&str, &str)> {
        rows.iter()
            .filter_map(|row| match row {
                Row::Item(label, value, _) | Row::Detail(label, value) => {
                    Some((*label, value.as_str()))
                }
                _ => None,
            })
            .collect()
    }

    /// The table names the console and the essential figures in a player's
    /// words, with their units; the times have bars of their share of the
    /// frame. Diagnostics builds add the waits for the card, the caches,
    /// shared memory, dynamic slots (with refused tables when there are) and
    /// the live switches set.
    #[test]
    fn rows_name_the_figures_with_their_units() {
        let (mut line, mut counts) = sample();
        let table = rows(&line, &counts, None);
        assert_eq!(table[0], Row::Title("Series X"));
        assert_eq!(table[1], Row::Big("5.7 fps".into(), "176 ms".into(), RED));
        assert_eq!(
            items(&table),
            [
                ("CPU prep", "102.6 ms"),
                ("GPU busy", "48.3 ms"),
                ("Copy-back", "56.8 ms"),
                ("Draws", "5030"),
                ("Dropped", "340"),
                ("VRAM", "14.99 / 22.82 GB"),
            ]
        );
        assert_eq!(table.iter().filter(|row| **row == Row::Rule).count(), 3);
        let Row::Item("CPU prep", _, Some(share)) = &table[3] else {
            panic!("{:?}", table[3]);
        };
        assert!((share - 102.6 / 176.0).abs() < 1e-9);

        let live = [("VICTORY_SCISSOR_TEST", "full".to_owned())];
        let more = rows(&line, &counts, Some(&live));
        assert_eq!(more[..table.len()], table[..]);
        assert_eq!(more[table.len()], Row::Rule);
        assert_eq!(
            items(&more[table.len()..]),
            [
                ("GPU wait", "0.4 ms"),
                ("Dispatches", "4783"),
                ("Shaders", "2319 + 4 new"),
                ("Pipelines", "1977 + 596 new"),
                ("Compiling", "0"),
                ("Shared memory", "5.50 GB"),
                ("Dynamic slots", "61234"),
                ("VICTORY_SCISSOR_TEST", "full"),
            ]
        );
        line.dynamic[1] = 12.0;
        assert!(
            items(&rows(&line, &counts, Some(&[])))
                .contains(&("Dynamic slots", "61234, 12 refused"))
        );

        counts.memory = None;
        assert!(items(&rows(&line, &counts, None)).contains(&("VRAM", "unknown")));
    }

    /// Each text shape's bounds in pixels.
    fn bounds(shape: &Shape) -> [u32; 4] {
        match shape {
            Shape::Fill(rect, _) => *rect,
            Shape::Text { at, face, text, .. } => {
                let [w, h] = FACES[*face].cell;
                [at[0], at[1], at[0] + text.len() as u32 * w, at[1] + h]
            }
        }
    }

    fn texts(shapes: &[Shape]) -> Vec<(&str, [u32; 4])> {
        shapes
            .iter()
            .filter_map(|shape| match shape {
                Shape::Text { text, .. } => Some((text.as_str(), bounds(shape))),
                Shape::Fill(..) => None,
            })
            .collect()
    }

    /// Labels start at the table's left edge and values end at its right
    /// edge, whatever their face; the table is as wide as its widest row
    /// needs, at least NARROWEST characters of the main face, at most the
    /// room.
    #[test]
    fn labels_and_values_line_up_in_every_face() {
        let (line, counts) = sample();
        let live = [("VICTORY_SKIP_PIPELINES", "0000000000000000".to_owned())];
        let shapes = layout(&rows(&line, &counts, Some(&live)), &FACES, 1000);
        let Shape::Fill(panel, BACKGROUND) = shapes[0] else {
            panic!("{:?}", shapes[0]);
        };
        let [cw, ch] = FACES[MAIN].cell;
        let (left, right) = (panel[0] + cw, panel[2] - cw);
        // The live switch is the widest row, in the small face.
        assert_eq!(right - left, 7 * (22 + 2 + 16));
        let placed = texts(&shapes);
        for label in ["VIctory-reloaded", "5.7 fps", "CPU prep", "VRAM", "Shaders"] {
            let (_, at) = placed.iter().find(|(t, _)| *t == label).unwrap();
            assert_eq!(at[0], left, "{label}");
        }
        for value in [
            "Series X",
            "176 ms",
            "102.6 ms",
            "14.99 / 22.82 GB",
            "2319 + 4 new",
        ] {
            let (_, at) = placed.iter().find(|(t, _)| *t == value).unwrap();
            assert_eq!(at[2], right, "{value}");
        }
        // Without it, the bars' rows: "Copy-back", a bar, "102.6 ms".
        let shapes = layout(&rows(&line, &counts, None), &FACES, 1000);
        let Shape::Fill(panel, _) = shapes[0] else {
            unreachable!()
        };
        assert_eq!(
            panel[2] - panel[0] - 2 * cw,
            (9 + 1 + BAR_CHARS + 1 + 8) as u32 * cw
        );
        let short = layout(&[Row::Item("Draws", "1".into(), None)], &FACES, 1000);
        let Shape::Fill(panel, _) = short[0] else {
            unreachable!()
        };
        assert_eq!(panel[2] - panel[0] - 2 * cw, NARROWEST as u32 * cw);
        assert_eq!(panel[..2], [ch / 2, ch / 2]);
        // A narrow window cuts values to leave a space after the label.
        let cut = layout(
            &[Row::Item("Pipelines", "9".repeat(40), None)],
            &FACES,
            15 * cw,
        );
        let placed = texts(&cut);
        assert_eq!(placed[0].0, "Pipelines");
        assert_eq!(placed[1].0, "99999");
        assert_eq!(placed[1].1[2], ch / 2 + cw + 15 * cw);
    }

    /// The panel comes first and holds every other shape with its padding;
    /// rules span the rows; bars lie between the label and value columns,
    /// filled by their share and never past full; a note is broken into
    /// lines of the table's width in the small face.
    #[test]
    fn the_panel_holds_rules_bars_and_notes() {
        let table = [
            Row::Title("Series S"),
            Row::Rule,
            Row::Item("CPU prep", "88.0 ms".into(), Some(0.5)),
            Row::Item("Copy-back", "300.0 ms".into(), Some(1.7)),
            Row::Rule,
            Row::Note(
                "view 3 0x7ff612345678 1920x1080 f10 rgb: values 0.0000e0 to 1.2500e1".into(),
            ),
        ];
        let shapes = layout(&table, &FACES, 1000);
        let Shape::Fill(panel, BACKGROUND) = shapes[0] else {
            panic!("{:?}", shapes[0]);
        };
        let [cw, ch] = FACES[MAIN].cell;
        let (left, right) = (panel[0] + cw, panel[2] - cw);
        for shape in &shapes[1..] {
            let [l, t, r, b] = bounds(shape);
            assert!(l >= left && r <= right, "{shape:?}");
            assert!(
                t >= panel[1] + ch / 3 && b <= panel[3] - ch / 3,
                "{shape:?}"
            );
        }
        let fills = |colour| -> Vec<[u32; 4]> {
            shapes[1..]
                .iter()
                .filter_map(|s| match s {
                    Shape::Fill(rect, c) if *c == colour => Some(*rect),
                    _ => None,
                })
                .collect()
        };
        let rules = fills(RULE);
        assert_eq!(rules.len(), 2);
        assert!(rules.iter().all(|r| r[0] == left && r[2] == right));
        // "Copy-back" is the longest label beside a bar, "300.0 ms" the
        // longest value.
        let (from, to) = (left + 10 * cw, right - 9 * cw);
        let (tracks, bars) = (fills(TRACK), fills(BAR));
        assert_eq!(tracks.len(), 2);
        assert!(tracks.iter().all(|r| r[0] == from && r[2] == to));
        assert_eq!(bars[0][2] - bars[0][0], (to - from) / 2);
        assert_eq!(bars[1][2], to);
        // After the title's and the bars' labels and values.
        let notes = &texts(&shapes)[6..];
        assert!(notes.len() > 1);
        let room = ((right - left) / FACES[SMALL].cell[0]) as usize;
        assert!(notes.iter().all(|(t, _)| t.len() <= room));
    }

    /// At 1080 rows the essential table takes less than a fifth of the
    /// window's width and a quarter of its height.
    #[test]
    fn the_table_is_compact() {
        let (line, counts) = sample();
        let shapes = layout(&rows(&line, &counts, None), &FACES, 1900);
        let Shape::Fill(panel, _) = shapes[0] else {
            unreachable!()
        };
        assert!(panel[2] <= 1920 / 5, "{panel:?}");
        assert!(panel[3] <= 1080 / 4, "{panel:?}");
    }
}
