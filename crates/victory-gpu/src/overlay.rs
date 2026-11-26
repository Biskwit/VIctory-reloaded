//! What the host draws over the presented frame, after the title's frame is
//! copied into the window's buffer and before it is presented
//! (`crate::host::Host::present`): the overlay, in every build, on unless
//! `VICTORY_OVERLAY=0` (in diagnostics builds, `overlay on|off` of
//! `crate::control` too). It is a table on a dark panel at the top left, in
//! a bitmap font of our own: frames a second and time a frame twice as
//! big, green from the 15 a second aimed at, amber from 10, red below; then
//! groups of rows, a grey label on the left and its value in white against
//! the right edge: where a frame's time goes, with bars of its share of the
//! frame; the work a frame; the shader and pipeline caches; the card's
//! memory; in diagnostics builds the live switches set (`crate::live`) and
//! the view's line ([`view`]).
//!
//! The shaders are `overlay.hlsl`, compiled by the build script. The rows
//! are made when the frame line's figures change, once a second
//! (`crate::host`'s meter); every present replays their draws, a rectangle
//! each: the panel, its lines and bars, and a draw a row of text.

use std::sync::atomic::{AtomicU8, Ordering::Relaxed};

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use crate::host::Host;
#[cfg(not(feature = "diagnostics"))]
use crate::host::HostList;

#[cfg(feature = "diagnostics")]
pub(crate) mod view;

const CORNERS: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/overlay-corners.dxil"));
const PANEL: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/overlay-panel.dxil"));

/// The frame line's figures over its last second (`crate::host`'s meter):
/// frames a second; milliseconds a frame, of them preparing, on the GPU and
/// copying back; draws, dispatches and drops a frame.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct FrameLine {
    pub(crate) fps: f64,
    pub(crate) ms: f64,
    pub(crate) cpu: f64,
    pub(crate) gpu: f64,
    pub(crate) back: f64,
    pub(crate) draws: f64,
    pub(crate) dispatches: f64,
    pub(crate) dropped: f64,
}

/// Characters of a line at most: the shader's eight words of four.
const LINE: usize = 128;

/// The shaders' constants (`Draw` in overlay.hlsl).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Constants {
    rect: [f32; 4],
    target: [f32; 2],
    colours: [u32; 2],
    head: [u32; 4],
    words: [u32; LINE / 4],
}

/// The root parameters: the constants (both root signatures), the font.
const DRAW: u32 = 0;
const FONT: u32 = 1;

/// A character's width in font pixels (`CELL_WIDTH` in overlay.hlsl), and
/// a glyph's height.
const CELL: u32 = 6;
const GLYPH: u32 = 8;

/// The table's measures in font pixels: from the window's corner to the
/// panel, from the panel's edges to the rows (across, down), a row's pitch
/// (its glyphs 2 down), the big row's, a rule's (its line 3 down).
const MARGIN: u32 = 6;
const PAD: [u32; 2] = [6, 4];
const ROW: u32 = 12;
const BIG_ROW: u32 = 20;
const RULE_ROW: u32 = 7;

/// The table's width in characters at least; a bar's; the value column
/// beside bars ("100.0 ms").
const NARROWEST: usize = 32;
const BAR_CHARS: usize = 8;
const BAR_VALUE: usize = 8;

/// A colour as the shader takes it: 8 bits a channel, red lowest, alpha
/// highest.
const fn rgba(r: u8, g: u8, b: u8, a: u8) -> u32 {
    u32::from_le_bytes([r, g, b, a])
}

const BACKGROUND: u32 = rgba(8, 10, 14, 216);
const RULE: u32 = rgba(255, 255, 255, 48);
const TRACK: u32 = rgba(255, 255, 255, 28);
const BAR: u32 = rgba(96, 165, 250, 255);
const LABEL: u32 = rgba(160, 168, 180, 255);
const VALUE: u32 = rgba(240, 242, 245, 255);
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
/// overlay: 8 rows a glyph, top first, its 5 columns in bits 4 (left) to 0.
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

/// A row of the table.
#[derive(Debug, Clone, PartialEq)]
enum Row {
    /// Text in white.
    Title(&'static str),
    /// Two figures at the row's ends, twice as big, in a colour.
    Big(String, String, u32),
    /// A label and its value against the right edge; with a share (0 to 1),
    /// a bar of it between them.
    Item(&'static str, String, Option<f64>),
    /// A thin line between groups.
    Rule,
    /// Grey text across the table, in lines that fit it.
    Note(String),
}

/// What the panel draws, in order, in the window's pixels.
#[derive(Debug, Clone, PartialEq)]
enum Shape {
    /// A rectangle (left, top, right, bottom) filled with a colour.
    Fill([u32; 4], u32),
    /// Text: its glyphs' top left, pixels a font pixel, the characters, the
    /// column from which they take the second colour, the colours.
    Text {
        at: [u32; 2],
        scale: u32,
        text: String,
        split: usize,
        colours: [u32; 2],
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
/// made: shaders found in the cache and translated, host pipelines loaded
/// from the library and made, those being made, the library's bytes, the
/// card's memory (`Host::video_memory`).
struct Counts {
    shaders: (u64, u64),
    pipelines: [u64; 2],
    making: u64,
    library: u64,
    memory: Option<[u64; 4]>,
}

impl Counts {
    fn now(host: &Host) -> Counts {
        Counts {
            shaders: crate::shader_cache::counts(),
            pipelines: crate::pipeline::library_pipelines(),
            making: crate::pipeline::making(),
            library: crate::pipeline::kept()[4],
            memory: host.video_memory(),
        }
    }
}

/// The table's rows: `line`'s figures, `counts`, and the live switches set.
fn rows(line: &FrameLine, counts: &Counts, live: &[(&'static str, String)]) -> Vec<Row> {
    const GB: f64 = (1u64 << 30) as f64;
    let ms = |ms: f64| format!("{ms:.1} ms");
    // Each against the whole frame: they overlap.
    let share = |part: f64| Some(if line.ms > 0.0 { part / line.ms } else { 0.0 });
    let (cached, translated) = counts.shaders;
    let [loaded, made] = counts.pipelines;
    let library = match counts.library >> 20 {
        0 => "none".to_owned(),
        mb => format!("{mb} MB"),
    };
    let mut rows = vec![
        Row::Title("VIctory-reloaded"),
        // Whole milliseconds: a clear gap after the frames a second.
        Row::Big(
            format!("{:.1} fps", line.fps),
            format!("{:.0} ms", line.ms),
            fps_colour(line.fps),
        ),
        Row::Rule,
        Row::Item("CPU prep", ms(line.cpu), share(line.cpu)),
        Row::Item("GPU", ms(line.gpu), share(line.gpu)),
        Row::Item("Copy-back", ms(line.back), share(line.back)),
        Row::Rule,
        Row::Item("Draws", format!("{:.0}", line.draws), None),
        Row::Item("Dispatches", format!("{:.0}", line.dispatches), None),
        Row::Item("Dropped", format!("{:.0}", line.dropped), None),
        Row::Rule,
        Row::Item(
            "Shaders",
            format!("{cached} cached, {translated} new"),
            None,
        ),
        Row::Item("Pipelines", format!("{loaded} cached, {made} new"), None),
        Row::Item("Compiling", format!("{} pipelines", counts.making), None),
        Row::Item("Pipeline cache", library, None),
        Row::Rule,
    ];
    match counts.memory {
        Some([budget, used, _, shared]) => {
            let gb = |bytes: u64| bytes as f64 / GB;
            rows.push(Row::Item(
                "VRAM",
                format!("{:.2} / {:.2} GB", gb(used), gb(budget)),
                None,
            ));
            rows.push(Row::Item(
                "Shared memory",
                format!("{:.2} GB", gb(shared)),
                None,
            ));
        }
        None => rows.push(Row::Item("VRAM", "unknown".to_owned(), None)),
    }
    if !live.is_empty() {
        rows.push(Row::Rule);
        rows.extend(
            live.iter()
                .map(|(name, value)| Row::Item(name, value.clone(), None)),
        );
    }
    rows
}

/// `left` and `right` on a line of `chars` characters, `right` against its
/// end (cut to leave a space after `left`, which is cut to the line), and
/// the column where `right` starts.
fn spread(left: &str, right: &str, chars: usize) -> (String, usize) {
    let left = &left[..left.len().min(chars)];
    let room = chars.saturating_sub(left.len() + 1);
    let right = &right[..right.len().min(room)];
    let pad = chars - left.len();
    (format!("{left}{right:>pad$}"), chars - right.len())
}

/// The shapes of `rows` at `scale` pixels a font pixel: the panel, then
/// each row's. The rows are as wide as the widest needs, at least
/// [`NARROWEST`] characters and an even count (the big row's halves), at
/// most `room` characters.
fn layout(rows: &[Row], scale: u32, room: usize) -> Vec<Shape> {
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
    let needs = |row: &Row| match row {
        Row::Title(title) => title.len(),
        Row::Big(left, right, _) => 2 * (left.len() + 1 + right.len()),
        Row::Item(label, value, None) => label.len() + 2 + value.len(),
        Row::Item(_, _, Some(_)) => bar_label + 1 + BAR_CHARS + 1 + bar_value,
        Row::Rule | Row::Note(_) => 0,
    };
    let width = rows
        .iter()
        .map(needs)
        .max()
        .unwrap_or(0)
        .max(NARROWEST)
        .next_multiple_of(2)
        .min(room.min(LINE))
        .max(1);
    let u = scale;
    let cell = CELL * u;
    let left = (MARGIN + PAD[0]) * u;
    // The last glyph's right edge: a cell's last column is a gap.
    let right = left + width as u32 * cell - u;
    // Bars run from a cell after the label column to a cell before the
    // value column.
    let from = left + (bar_label + 1) as u32 * cell;
    let to = left + width.saturating_sub(bar_value + 1) as u32 * cell;
    let text = |y: u32, scale: u32, (text, split): (String, usize), colours| Shape::Text {
        at: [left, y + 2 * u],
        scale,
        text,
        split,
        colours,
    };
    let mut shapes = vec![Shape::Fill([0; 4], BACKGROUND)];
    let mut y = (MARGIN + PAD[1]) * u;
    for row in rows {
        match row {
            Row::Title(title) => {
                let title = printable(title);
                shapes.push(text(y, u, (title.clone(), title.len()), [VALUE; 2]));
                y += ROW * u;
            }
            Row::Big(l, r, colour) => {
                let line = spread(&printable(l), &printable(r), width / 2);
                shapes.push(text(y, 2 * u, line, [*colour; 2]));
                y += BIG_ROW * u;
            }
            Row::Item(label, value, bar) => {
                if let Some(share) = bar
                    && to > from
                {
                    let (top, bottom) = (y + 3 * u, y + 8 * u);
                    shapes.push(Shape::Fill([from, top, to, bottom], TRACK));
                    let filled = (f64::from(to - from) * share.clamp(0.0, 1.0)).round() as u32;
                    if filled > 0 {
                        shapes.push(Shape::Fill([from, top, from + filled, bottom], BAR));
                    }
                }
                let line = spread(&printable(label), &printable(value), width);
                shapes.push(text(y, u, line, [LABEL, VALUE]));
                y += ROW * u;
            }
            Row::Rule => {
                let top = y + 3 * u;
                shapes.push(Shape::Fill([left, top, right, top + u.div_ceil(2)], RULE));
                y += RULE_ROW * u;
            }
            Row::Note(note) => {
                for line in wrap(note, width) {
                    let split = line.len();
                    shapes.push(text(y, u, (line, split), [LABEL; 2]));
                    y += ROW * u;
                }
            }
        }
    }
    shapes[0] = Shape::Fill(
        [MARGIN * u, MARGIN * u, right + PAD[0] * u, y + PAD[1] * u],
        BACKGROUND,
    );
    shapes
}

/// The shaders' constants for `shape` in a window of `size`.
fn constants(shape: &Shape, size: [u32; 2]) -> Constants {
    let target = size.map(|s| s as f32);
    match shape {
        Shape::Fill(rect, colour) => Constants {
            rect: rect.map(|v| v as f32),
            target,
            colours: [*colour, 0],
            ..Default::default()
        },
        Shape::Text {
            at: [x, y],
            scale,
            text,
            split,
            colours,
        } => {
            let (words, count) = pack(text);
            let (right, bottom) = (x + count * CELL * scale, y + GLYPH * scale);
            Constants {
                rect: [*x as f32, *y as f32, right as f32, bottom as f32],
                target,
                colours: *colours,
                head: [*scale, count, *split as u32, 0],
                words,
            }
        }
    }
}

/// The panel's draws in a window of `size`, its font pixels a 540th of its
/// height: `line`'s table (with the counts as they are now), if any, and
/// `note` under it.
fn draws(
    host: &Host,
    line: Option<&FrameLine>,
    size: [u32; 2],
    note: Option<&str>,
) -> Vec<Constants> {
    let mut table = match line {
        Some(line) => {
            #[cfg(feature = "diagnostics")]
            let live = crate::live::active();
            #[cfg(not(feature = "diagnostics"))]
            let live: Vec<(&'static str, String)> = Vec::new();
            rows(line, &Counts::now(host), &live)
        }
        None => Vec::new(),
    };
    if let Some(note) = note {
        if !table.is_empty() {
            table.push(Row::Rule);
        }
        table.push(Row::Note(note.to_owned()));
    }
    let scale = (size[1] / 540).max(1);
    let room = (size[0] / scale).saturating_sub(2 * (MARGIN + PAD[0])) / CELL;
    layout(&table, scale, room as usize)
        .iter()
        .map(|shape| constants(shape, size))
        .collect()
}

/// Pipelines that draw rectangles under `root` with a vertex and a pixel
/// shader, blending by alpha or not, made for a window buffer's format at
/// its first use (`None`: the host refused it).
struct Pipelines {
    root: ID3D12RootSignature,
    shaders: [&'static [u8]; 2],
    blend: bool,
    made: Vec<(DXGI_FORMAT, Option<ID3D12PipelineState>)>,
}

impl Pipelines {
    fn new(root: ID3D12RootSignature, shaders: [&'static [u8]; 2], blend: bool) -> Pipelines {
        Pipelines {
            root,
            shaders,
            blend,
            made: Vec::new(),
        }
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
        let [vs, ps] = self.shaders;
        // SAFETY: the description points at live data and never releases
        // its pointer.
        let made = unsafe {
            host.device
                .CreateGraphicsPipelineState(&D3D12_GRAPHICS_PIPELINE_STATE_DESC {
                    pRootSignature: std::mem::transmute_copy(&self.root),
                    VS: bytecode(vs),
                    PS: bytecode(ps),
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
    /// The font ([`GLYPHS`]), in upload memory.
    font: ID3D12Resource,
    /// The panel's draws as last made.
    drawn: Option<Drawn>,
    /// The view's objects, made at the first view (`None` inside: none
    /// could be made).
    #[cfg(feature = "diagnostics")]
    view: Option<Option<view::View>>,
}

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
        if CORNERS.is_empty() || PANEL.is_empty() {
            tracing::warn!(
                "no overlay: this build has no shaders for it (no dxc when it was built)"
            );
            return None;
        }
        match Self::make(host) {
            Ok(made) => made,
            Err(e) => {
                tracing::warn!(error = %e, "no overlay");
                None
            }
        }
    }

    fn make(host: &Host) -> windows::core::Result<Option<Overlay>> {
        let Some((font, data)) = host.upload_buffer(GLYPHS.len() as u64 * 8) else {
            return Ok(None);
        };
        // SAFETY: the upload buffer's mapping holds the font's bytes; the
        // shaders' blob carries the root signature.
        unsafe {
            std::ptr::copy_nonoverlapping(GLYPHS.as_ptr().cast::<u8>(), data, GLYPHS.len() * 8);
            let root: ID3D12RootSignature = host.device.CreateRootSignature(0, CORNERS)?;
            crate::host::name(&font, || "overlay font".to_owned());
            tracing::info!("overlay made");
            Ok(Some(Overlay {
                pipelines: Pipelines::new(root, [CORNERS, PANEL], true),
                font,
                drawn: None,
                #[cfg(feature = "diagnostics")]
                view: None,
            }))
        }
    }

    /// Records the panel at the top left of the window buffer `back` (in the
    /// render target state): `line`'s table, if any, and `note` under it.
    /// Its draws are made again when either changed, or the window's size.
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
                draws: draws(host, line.as_ref(), size, note),
            },
        };
        let drawn = self.drawn.insert(drawn);
        let words = (size_of::<Constants>() / 4) as u32;
        // SAFETY: an open list; our root signature and font, which live as
        // long as the overlay; a view of the window buffer from the host's
        // ring, made where it is recorded (the ring comes round).
        unsafe {
            list.SetGraphicsRootSignature(&self.pipelines.root);
            list.SetGraphicsRootShaderResourceView(FONT, self.font.GetGPUVirtualAddress());
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
/// the overlay of `line` if it is on. `made` holds its objects.
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

    /// The shader finds a glyph's row as overlay.hlsl does: the word at the
    /// glyph's 8 bytes plus the row's group of four, shifted by the row.
    #[test]
    fn rows_are_where_the_shader_reads_them() {
        let bytes: Vec<u8> = GLYPHS.iter().flatten().copied().collect();
        let row_of = |c: u8, row: usize| {
            let at = usize::from(c - 32) * 8 + (row & !3);
            let word = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
            (word >> ((row & 3) * 8)) & 0x1f
        };
        // The capital A: a top bar, its sides, the crossbar on row 4.
        let a: Vec<String> = (0..8)
            .map(|row| {
                (0..5)
                    .map(|x| {
                        if row_of(b'A', row) >> (4 - x) & 1 != 0 {
                            '#'
                        } else {
                            '.'
                        }
                    })
                    .collect()
            })
            .collect();
        assert_eq!(
            a,
            [
                ".###.", "#...#", "#...#", "#...#", "#####", "#...#", "#...#", "....."
            ]
        );
        // A descender on the last row.
        assert_ne!(row_of(b'g', 7), 0);
    }

    #[test]
    fn lines_pack_four_characters_a_word() {
        let (words, count) = pack("AB\u{e9}~x");
        assert_eq!(count, 5);
        assert_eq!(words[0], u32::from_le_bytes([b'A', b'B', b'?', b'~']));
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

    /// The constants are the 44 words both root signatures of overlay.hlsl
    /// give them.
    #[test]
    fn the_constants_are_the_root_constants() {
        assert_eq!(size_of::<Constants>(), 44 * 4);
        let hlsl = include_str!("overlay.hlsl");
        assert_eq!(hlsl.matches("num32BitConstants = 44, b0").count(), 2);
    }

    fn sample() -> (FrameLine, Counts) {
        let gb = |gb: f64| (gb * (1u64 << 30) as f64) as u64;
        let line = FrameLine {
            fps: 5.7,
            ms: 176.0,
            cpu: 102.6,
            gpu: 48.3,
            back: 56.8,
            draws: 5030.4,
            dispatches: 4783.0,
            dropped: 340.0,
        };
        let counts = Counts {
            shaders: (2319, 4),
            pipelines: [1977, 596],
            making: 0,
            library: 64 << 20,
            memory: Some([gb(22.82), gb(14.99), 0, gb(5.5)]),
        };
        (line, counts)
    }

    fn items(rows: &[Row]) -> Vec<(&str, &str)> {
        rows.iter()
            .filter_map(|row| match row {
                Row::Item(label, value, _) => Some((*label, value.as_str())),
                _ => None,
            })
            .collect()
    }

    /// The rows name the figures in a player's words, with their units, in
    /// groups apart; the times have bars of their share of the frame; live
    /// switches set make a group of their own.
    #[test]
    fn rows_name_the_figures_with_their_units() {
        let (line, mut counts) = sample();
        let table = rows(&line, &counts, &[]);
        assert_eq!(table[0], Row::Title("VIctory-reloaded"));
        assert_eq!(table[1], Row::Big("5.7 fps".into(), "176 ms".into(), RED));
        assert_eq!(
            items(&table),
            [
                ("CPU prep", "102.6 ms"),
                ("GPU", "48.3 ms"),
                ("Copy-back", "56.8 ms"),
                ("Draws", "5030"),
                ("Dispatches", "4783"),
                ("Dropped", "340"),
                ("Shaders", "2319 cached, 4 new"),
                ("Pipelines", "1977 cached, 596 new"),
                ("Compiling", "0 pipelines"),
                ("Pipeline cache", "64 MB"),
                ("VRAM", "14.99 / 22.82 GB"),
                ("Shared memory", "5.50 GB"),
            ]
        );
        assert_eq!(table.iter().filter(|row| **row == Row::Rule).count(), 4);
        let Row::Item("CPU prep", _, Some(share)) = &table[3] else {
            panic!("{:?}", table[3]);
        };
        assert!((share - 102.6 / 176.0).abs() < 1e-9);
        assert!(
            table
                .iter()
                .all(|row| !matches!(row, Row::Item("Draws", _, Some(_))))
        );

        let live = [("VICTORY_SCISSOR_TEST", "full".to_owned())];
        let with = rows(&line, &counts, &live);
        assert_eq!(with.len(), table.len() + 2);
        assert_eq!(with[table.len()], Row::Rule);
        assert_eq!(
            with[table.len() + 1],
            Row::Item("VICTORY_SCISSOR_TEST", "full".into(), None)
        );

        counts.memory = None;
        counts.library = 0;
        let unknown = rows(&line, &counts, &[]);
        assert!(items(&unknown).contains(&("VRAM", "unknown")));
        assert!(items(&unknown).contains(&("Pipeline cache", "none")));
    }

    /// A value stands against the row's right edge, its label at the left;
    /// what does not fit is cut, the label last.
    #[test]
    fn values_stand_against_the_right_edge() {
        assert_eq!(
            spread("GPU", "48.3 ms", 16),
            ("GPU      48.3 ms".to_owned(), 9)
        );
        assert_eq!(
            spread("Dispatches", "4783", 15),
            ("Dispatches 4783".to_owned(), 11)
        );
        assert_eq!(
            spread("Dispatches", "4783", 14),
            ("Dispatches 478".to_owned(), 11)
        );
        assert_eq!(spread("A long label", "1", 6), ("A long".to_owned(), 6));
        assert_eq!(spread("Title", "", 8), ("Title   ".to_owned(), 8));
    }

    fn texts(shapes: &[Shape]) -> Vec<(&str, usize, u32)> {
        shapes
            .iter()
            .filter_map(|shape| match shape {
                Shape::Text {
                    text, split, scale, ..
                } => Some((text.as_str(), *split, *scale)),
                Shape::Fill(..) => None,
            })
            .collect()
    }

    /// Every row is as wide as the widest needs: at least NARROWEST
    /// characters, an even count, at most the room; values end at its right
    /// edge, where their colour starts.
    #[test]
    fn rows_share_one_width() {
        let (line, counts) = sample();
        let shapes = layout(&rows(&line, &counts, &[]), 2, 200);
        for (text, split, scale) in texts(&shapes) {
            if text == "VIctory-reloaded" {
                continue;
            }
            assert_eq!(text.len() * (scale / 2) as usize, NARROWEST, "{text:?}");
            assert_eq!(text.as_bytes()[split - 1], b' ', "{text:?}");
            assert_ne!(text.as_bytes()[split], b' ', "{text:?}");
        }
        let wide = layout(
            &[Row::Item(
                "Shaders",
                "123456 cached, 12345 new".into(),
                None,
            )],
            1,
            200,
        );
        assert_eq!(
            texts(&wide),
            [("Shaders   123456 cached, 12345 new", 10, 1)]
        );
        let capped = layout(&[Row::Item("Shaders", "9".repeat(60), None)], 1, 41);
        assert_eq!(texts(&capped)[0].0.len(), 41);
        let big = layout(
            &[Row::Big("144.0 fps".into(), "6.9 ms".into(), GREEN)],
            1,
            200,
        );
        assert_eq!(texts(&big), [("144.0 fps 6.9 ms", 10, 2)]);
    }

    /// The panel comes first and holds every other shape with its padding
    /// around them; rules span the rows; bars lie between the label and
    /// value columns, filled by their share and never past full; a note
    /// is broken into lines of the table's width.
    #[test]
    fn the_panel_holds_rules_bars_and_notes() {
        let u = 2;
        let table = [
            Row::Title("VIctory-reloaded"),
            Row::Rule,
            Row::Item("CPU prep", "88.0 ms".into(), Some(0.5)),
            Row::Item("Copy-back", "300.0 ms".into(), Some(1.7)),
            Row::Rule,
            Row::Note(
                "view 3 0x7ff612345678 1920x1080 f10 rgb: values 0.0000e0 to 1.2500e1".into(),
            ),
        ];
        let shapes = layout(&table, u, 200);
        let Shape::Fill(panel, BACKGROUND) = shapes[0] else {
            panic!("{:?}", shapes[0]);
        };
        let (left, cell) = ((MARGIN + PAD[0]) * u, CELL * u);
        let right = left + NARROWEST as u32 * cell - u;
        assert_eq!(panel[..3], [MARGIN * u, MARGIN * u, right + PAD[0] * u]);
        let fills: Vec<([u32; 4], u32)> = shapes[1..]
            .iter()
            .filter_map(|shape| match shape {
                Shape::Fill(rect, colour) => Some((*rect, *colour)),
                Shape::Text { .. } => None,
            })
            .collect();
        for shape in &shapes[1..] {
            let [l, t, r, b] = match shape {
                Shape::Fill(rect, _) => *rect,
                Shape::Text {
                    at, scale, text, ..
                } => [
                    at[0],
                    at[1],
                    at[0] + text.len() as u32 * CELL * scale - scale,
                    at[1] + GLYPH * scale,
                ],
            };
            assert!(
                l >= panel[0] + PAD[0] * u && r <= panel[2] - PAD[0] * u,
                "{shape:?}"
            );
            assert!(
                t >= panel[1] + PAD[1] * u && b <= panel[3] - PAD[1] * u,
                "{shape:?}"
            );
        }
        let rules: Vec<_> = fills.iter().filter(|(_, c)| *c == RULE).collect();
        assert_eq!(rules.len(), 2);
        assert!(
            rules
                .iter()
                .all(|(rect, _)| rect[0] == left && rect[2] == right)
        );
        // "Copy-back" is the longest label beside a bar, "300.0 ms" the
        // longest value.
        let (from, to) = (left + 10 * cell, left + (NARROWEST as u32 - 9) * cell);
        let tracks: Vec<_> = fills
            .iter()
            .filter(|(_, c)| *c == TRACK)
            .map(|(r, _)| *r)
            .collect();
        let bars: Vec<_> = fills
            .iter()
            .filter(|(_, c)| *c == BAR)
            .map(|(r, _)| *r)
            .collect();
        assert_eq!(tracks.len(), 2);
        assert!(tracks.iter().all(|r| r[0] == from && r[2] == to));
        assert_eq!(bars[0][2] - bars[0][0], (to - from) / 2);
        assert_eq!(bars[1][2], to);
        let notes: Vec<_> = texts(&shapes).into_iter().skip(3).collect();
        assert!(notes.len() > 1);
        assert!(
            notes
                .iter()
                .all(|(text, split, _)| text.len() <= NARROWEST && *split == text.len())
        );
    }
}
