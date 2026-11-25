//! Diagnostics: what the host draws over the presented frame, after the
//! title's frame is copied into the window's buffer and before it is
//! presented (`crate::host::Host::present`), as `crate::control` asks:
//! - the overlay (`overlay on`, or `VICTORY_OVERLAY=1`): lines of counters
//!   in a bitmap font of our own: the frame line's times (`crate::host`),
//!   draws, dispatches and drops, the shader cache and pipelines, the
//!   card's memory, the live switches that have a value (`crate::live`);
//! - a view (`view <n>`): one texture of the frame in place of the frame,
//!   its values brought to what a screen shows as target dumps are
//!   (`crate::capture`), from the range of the channels shown, which a
//!   compute pass over the texture finds first. The range found is read
//!   back and named on the overlay at the next present.
//!
//! The shaders are `overlay.hlsl`, compiled by the build script. Nothing is
//! made or recorded while both are off. The overlay alone costs a draw of
//! one rectangle a line; a view makes its present wait for the card (its
//! views and range buffer are used again at the next one).

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use crate::control::{AtPresent, Channel, Viewed};
use crate::host::{Host, HostList};

const CORNERS: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/overlay-corners.dxil"));
const TEXT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/overlay-text.dxil"));
const VIEW: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/overlay-view.dxil"));
const RESET: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/overlay-reset.dxil"));
const REDUCE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/overlay-reduce.dxil"));

/// The frame line's figures over its last second (`crate::host`'s meter):
/// frames a second; milliseconds a frame, of them preparing, on the GPU and
/// copying back; draws, dispatches and drops a frame.
#[derive(Clone, Copy, Debug, Default)]
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

/// The shader's constants (`Draw` in overlay.hlsl).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Constants {
    rect: [f32; 4],
    target: [f32; 2],
    head: [u32; 2],
    words: [u32; LINE / 4],
}

/// The root signature's parameters: the constants, the font, the range,
/// the table of the viewed texture.
const DRAW: u32 = 0;
const FONT: u32 = 1;
const RANGE: u32 = 2;
const TEXTURE: u32 = 3;

/// How a view shows values (`SHOW_*` in overlay.hlsl); 0: as they are.
const SHOW_RANGE: u32 = 1;
const SHOW_HIGHEST: u32 = 2;
const SHOW_CURVE: u32 = 4;

/// A character's cell in font pixels (`CELL_*` in overlay.hlsl).
const CELL: [u32; 2] = [6, 10];

/// Texels a side the range pass reads at most: a step between them past that.
const RANGE_GRID: u32 = 2048;

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

/// How a view reads a texture of host format `format` and shows it: the
/// view's format, whether it holds integers, the channel of a format of one
/// channel (red, or alpha for A8), and how values are shown (`SHOW_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Reading {
    format: u32,
    integers: bool,
    single: Option<u32>,
    show: u32,
}

/// [`Reading`] for host format `format`, as dumps show it
/// (`crate::capture`): 8-bit and compressed colour as it is; other colour
/// scaled by its highest value, floats of light through a cube root; a
/// single channel and depth from their lowest to their highest. Typeless
/// formats are read as their plain kind (depth as its depth plane). `None`
/// for formats not shown (signed integers, typed depth, video).
fn reading(format: u32) -> Option<Reading> {
    const LIGHT: u32 = SHOW_HIGHEST | SHOW_CURVE;
    let colour = |format, show| Reading {
        format,
        integers: false,
        single: None,
        show,
    };
    let integers = |format| Reading {
        format,
        integers: true,
        single: None,
        show: SHOW_HIGHEST,
    };
    let single = |format, integers| Reading {
        format,
        integers,
        single: Some(0),
        show: SHOW_RANGE,
    };
    Some(match format {
        1 | 2 => colour(2, LIGHT),
        3 => integers(3),
        5 | 6 => colour(6, LIGHT),
        7 => integers(7),
        9 | 10 => colour(10, LIGHT),
        11 | 13 => colour(format, SHOW_HIGHEST),
        12 => integers(12),
        15 | 16 => colour(16, SHOW_HIGHEST),
        17 => integers(17),
        // Depth planes: R32G8X24, R24G8, R16 typeless (and their views).
        19 | 21 => single(21, false),
        23 | 24 | 89 => colour(if format == 23 { 24 } else { format }, SHOW_HIGHEST),
        25 => integers(25),
        26 | 67 => colour(format, LIGHT),
        27 | 28 => colour(28, 0),
        29 | 31 => colour(format, 0),
        30 => integers(30),
        33 | 34 => colour(34, SHOW_HIGHEST),
        35 | 37 => colour(format, SHOW_HIGHEST),
        36 => integers(36),
        39 | 41 => single(41, false),
        42 => single(42, true),
        44 | 46 => single(46, false),
        48 | 49 => colour(49, 0),
        50 => integers(50),
        51 => colour(51, 0),
        53 | 56 => single(56, false),
        54 | 58 => single(format, false),
        57 => single(57, true),
        60 | 61 => single(61, false),
        62 => single(62, true),
        63 => single(63, false),
        65 => Reading {
            single: Some(3),
            ..single(65, false)
        },
        // Block-compressed: typeless as UNORM; BC4 has one channel; BC6H
        // holds light.
        70..=78 | 82..=84 | 97..=99 => colour(
            if matches!(format, 70 | 73 | 76 | 82 | 97) {
                format + 1
            } else {
                format
            },
            0,
        ),
        79..=81 => Reading {
            single: Some(0),
            ..colour(if format == 79 { 80 } else { format }, 0)
        },
        94..=96 => colour(if format == 94 { 95 } else { format }, LIGHT),
        85 | 86 | 87 | 88 | 91 | 93 | 115 => colour(format, 0),
        90 | 92 => colour(format - 3, 0),
        _ => return None,
    })
}

/// The channel shown (0-3; 4: red, green and blue) and how, for `channel`
/// asked of a texture read as `r`: one channel asked for is shown as grey,
/// from its lowest to its highest unless the format is shown as it is.
fn shown(r: Reading, channel: Channel) -> (u32, u32) {
    let one = match channel {
        Channel::R => 0,
        Channel::G => 1,
        Channel::B => 2,
        Channel::A => 3,
        Channel::Rgb => return (r.single.unwrap_or(4), r.show),
    };
    let show = if r.show == 0 {
        0
    } else {
        SHOW_RANGE | r.show & SHOW_CURVE
    };
    (one, show)
}

/// A float from the range pass's key (`unkey` in overlay.hlsl).
fn unkey(k: u32) -> f32 {
    f32::from_bits(if k & 0x8000_0000 != 0 {
        k & 0x7fff_ffff
    } else {
        !k
    })
}

/// What a present still has to do once its list has run ([`finish`]).
pub(crate) struct After {
    shot: Option<(String, ID3D12Resource, D3D12_PLACED_SUBRESOURCE_FOOTPRINT)>,
    range: bool,
}

/// The objects the overlay and views draw with, made at their first use.
pub(crate) struct Overlay {
    root: ID3D12RootSignature,
    /// By the window buffer's format: the text and the view pipelines
    /// (`None`: the host refused them).
    graphics: Vec<(DXGI_FORMAT, Option<[ID3D12PipelineState; 2]>)>,
    reset: ID3D12PipelineState,
    reduce: ID3D12PipelineState,
    /// The font ([`GLYPHS`]), in upload memory.
    font: ID3D12Resource,
    /// The range pass's lowest and highest key, where it is read back to,
    /// and what that held after the last view.
    range: ID3D12Resource,
    range_back: ID3D12Resource,
    last_range: Option<(String, f32, f32)>,
    /// The label of the view whose range is being read back.
    ranging: String,
    /// The viewed texture's two views (floats, integers).
    views: ID3D12DescriptorHeap,
    views_cpu: usize,
    views_gpu: u64,
    view_step: u32,
}

impl Overlay {
    fn new(host: &Host) -> Option<Overlay> {
        if [CORNERS, TEXT, VIEW, RESET, REDUCE]
            .iter()
            .any(|code| code.is_empty())
        {
            tracing::warn!(
                "no overlay or view: this build has no shaders for them (no dxc when it was built)"
            );
            return None;
        }
        match Self::make(host) {
            Ok(made) => made,
            Err(e) => {
                tracing::warn!(error = %e, "no overlay or view");
                None
            }
        }
    }

    fn make(host: &Host) -> windows::core::Result<Option<Overlay>> {
        let device = &host.device;
        let (Some((font, data)), Some(range), Some(range_back)) = (
            host.upload_buffer(GLYPHS.len() as u64 * 8),
            host.uav_buffer(8),
            readback(host, 8),
        ) else {
            return Ok(None);
        };
        // SAFETY: the upload buffer's mapping holds the font's bytes; the
        // shaders' blobs carry the root signature; the descriptions point
        // at live data and never release their pointers.
        unsafe {
            std::ptr::copy_nonoverlapping(GLYPHS.as_ptr().cast::<u8>(), data, GLYPHS.len() * 8);
            let root: ID3D12RootSignature = device.CreateRootSignature(0, CORNERS)?;
            let compute = |code: &[u8]| -> windows::core::Result<ID3D12PipelineState> {
                device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                    pRootSignature: std::mem::transmute_copy(&root),
                    CS: bytecode(code),
                    ..Default::default()
                })
            };
            let (reset, reduce) = (compute(RESET)?, compute(REDUCE)?);
            let kind = D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV;
            let views: ID3D12DescriptorHeap =
                device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: kind,
                    NumDescriptors: 2,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                    NodeMask: 0,
                })?;
            let overlay = Overlay {
                root,
                graphics: Vec::new(),
                reset,
                reduce,
                font,
                range,
                range_back,
                last_range: None,
                ranging: String::new(),
                views_cpu: views.GetCPUDescriptorHandleForHeapStart().ptr,
                views_gpu: views.GetGPUDescriptorHandleForHeapStart().ptr,
                view_step: device.GetDescriptorHandleIncrementSize(kind),
                views,
            };
            // Null until a view: the text binds the table too.
            overlay.texture_views(host, None);
            crate::host::name(&overlay.font, || "overlay font".to_owned());
            tracing::info!("overlay and view made");
            Ok(Some(overlay))
        }
    }

    /// The text and view pipelines for a window buffer of `format`.
    fn pipelines(&mut self, host: &Host, format: DXGI_FORMAT) -> Option<[ID3D12PipelineState; 2]> {
        if let Some((_, made)) = self.graphics.iter().find(|(f, _)| *f == format) {
            return made.clone();
        }
        let make = |code: &[u8], blend: bool| -> windows::core::Result<ID3D12PipelineState> {
            let mut targets = D3D12_BLEND_DESC::default();
            targets.RenderTarget[0] = D3D12_RENDER_TARGET_BLEND_DESC {
                BlendEnable: blend.into(),
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
            // SAFETY: the description points at live data and never
            // releases its pointer.
            unsafe {
                host.device
                    .CreateGraphicsPipelineState(&D3D12_GRAPHICS_PIPELINE_STATE_DESC {
                        pRootSignature: std::mem::transmute_copy(&self.root),
                        VS: bytecode(CORNERS),
                        PS: bytecode(code),
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
            }
        };
        let made = match (make(TEXT, true), make(VIEW, false)) {
            (Ok(text), Ok(view)) => Some([text, view]),
            (Err(e), _) | (_, Err(e)) => {
                tracing::warn!(error = %e, format = format.0, "no overlay pipelines for this window format");
                None
            }
        };
        self.graphics.push((format, made.clone()));
        made
    }

    /// Writes the table's two views: of `texture` read as `r` (in the slot
    /// of its kind) and a null view, or two null views.
    fn texture_views(&self, host: &Host, texture: Option<(&ID3D12Resource, Reading)>) {
        let desc = |format: DXGI_FORMAT| D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: format,
            ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2DARRAY,
            Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture2DArray: D3D12_TEX2D_ARRAY_SRV {
                    MostDetailedMip: 0,
                    MipLevels: 1,
                    FirstArraySlice: 0,
                    ArraySize: 1,
                    ..Default::default()
                },
            },
        };
        let slot = |i: u32| D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: self.views_cpu + (i * self.view_step) as usize,
        };
        let nulls = [DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_FORMAT_R32G32B32A32_UINT];
        for (i, null) in nulls.into_iter().enumerate() {
            let mine = texture.filter(|(_, r)| usize::from(r.integers) == i);
            // SAFETY: a slot of our heap; a live texture, or a null view.
            unsafe {
                match mine {
                    Some((texture, r)) => host.device.CreateShaderResourceView(
                        texture,
                        Some(&desc(DXGI_FORMAT(r.format as i32))),
                        slot(i as u32),
                    ),
                    None => host.device.CreateShaderResourceView(
                        None::<&ID3D12Resource>,
                        Some(&desc(null)),
                        slot(i as u32),
                    ),
                }
            }
        }
    }

    /// Binds the root signature and its arguments for graphics (or compute).
    fn bind(&self, list: &ID3D12GraphicsCommandList, constants: &Constants, graphics: bool) {
        let words = (size_of::<Constants>() / 4) as u32;
        let table = D3D12_GPU_DESCRIPTOR_HANDLE {
            ptr: self.views_gpu,
        };
        // SAFETY: an open list; our root signature, heap and buffers, which
        // live as long as the overlay.
        unsafe {
            list.SetDescriptorHeaps(&[Some(self.views.clone())]);
            let constants = std::ptr::from_ref(constants).cast();
            if graphics {
                list.SetGraphicsRootSignature(&self.root);
                list.SetGraphicsRoot32BitConstants(DRAW, words, constants, 0);
                list.SetGraphicsRootShaderResourceView(FONT, self.font.GetGPUVirtualAddress());
                list.SetGraphicsRootUnorderedAccessView(RANGE, self.range.GetGPUVirtualAddress());
                list.SetGraphicsRootDescriptorTable(TEXTURE, table);
            } else {
                list.SetComputeRootSignature(&self.root);
                list.SetComputeRoot32BitConstants(DRAW, words, constants, 0);
                list.SetComputeRootShaderResourceView(FONT, self.font.GetGPUVirtualAddress());
                list.SetComputeRootUnorderedAccessView(RANGE, self.range.GetGPUVirtualAddress());
                list.SetComputeRootDescriptorTable(TEXTURE, table);
            }
        }
    }

    /// Records the view of `viewed` over the whole of the window buffer
    /// (`rtv`, `size`, `format`; in the render target state): the range
    /// pass, the texture scaled to fit, the range's read back.
    fn view(
        &mut self,
        host: &Host,
        list: &ID3D12GraphicsCommandList,
        rtv: D3D12_CPU_DESCRIPTOR_HANDLE,
        format: DXGI_FORMAT,
        size: [u32; 2],
        viewed: &Viewed,
    ) -> Result<(), String> {
        use std::sync::atomic::Ordering::Relaxed;
        let texture = &viewed.copy.texture;
        // SAFETY: a live resource.
        let d = unsafe { texture.GetDesc() };
        if d.Dimension != D3D12_RESOURCE_DIMENSION_TEXTURE2D || d.SampleDesc.Count > 1 {
            return Err("not shown: not a 2D texture of one sample".to_owned());
        }
        if d.Flags.0 & D3D12_RESOURCE_FLAG_DENY_SHADER_RESOURCE.0 != 0 {
            return Err("not shown: shaders may not read it".to_owned());
        }
        let r = reading(d.Format.0 as u32)
            .ok_or_else(|| format!("not shown: format {}", d.Format.0))?;
        let [_, view] = self.pipelines(host, format).ok_or("no pipelines")?;
        let (channel, show) = shown(r, viewed.channel);
        let (width, height) = (d.Width as u32, d.Height);
        let step = width.max(height).div_ceil(RANGE_GRID).max(1);
        // The texture scaled to fit the window, in its middle.
        let scale = (size[0] as f32 / width as f32).min(size[1] as f32 / height as f32);
        let (w, h) = (width as f32 * scale, height as f32 * scale);
        let (left, top) = ((size[0] as f32 - w) / 2.0, (size[1] as f32 - h) / 2.0);
        let mut constants = Constants {
            rect: [left, top, left + w, top + h],
            target: [size[0] as f32, size[1] as f32],
            head: [channel, show],
            ..Default::default()
        };
        constants.words[..4].copy_from_slice(&[width, height, u32::from(r.integers), step]);
        self.texture_views(host, Some((texture, r)));
        self.ranging.clone_from(&viewed.label);
        let state = D3D12_RESOURCE_STATES(crate::host::legal_state(
            texture,
            viewed.copy.state.load(Relaxed),
        ) as i32);
        let read = D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE
            | D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE;
        let uav = D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                    // SAFETY: a borrowed pointer the barrier never releases.
                    pResource: unsafe { std::mem::transmute_copy(&self.range) },
                }),
            },
        };
        let common = D3D12_RESOURCE_STATE_COMMON;
        // SAFETY: an open list; live resources; every state as recorded.
        unsafe {
            let mut barriers = vec![crate::host::transition(
                &self.range,
                common,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            )];
            if state != read {
                barriers.push(crate::host::transition(texture, state, read));
            }
            list.ResourceBarrier(&barriers);
            self.bind(list, &constants, false);
            list.SetPipelineState(&self.reset);
            list.Dispatch(1, 1, 1);
            list.ResourceBarrier(std::slice::from_ref(&uav));
            list.SetPipelineState(&self.reduce);
            list.Dispatch(
                width.div_ceil(step).div_ceil(8),
                height.div_ceil(step).div_ceil(8),
                1,
            );
            list.ResourceBarrier(std::slice::from_ref(&uav));
            self.bind(list, &constants, true);
            target(list, rtv, size);
            list.ClearRenderTargetView(rtv, &[0.04, 0.04, 0.06, 1.0], None);
            list.SetPipelineState(&view);
            list.DrawInstanced(4, 1, 0, 0);
            let mut barriers = vec![crate::host::transition(
                &self.range,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            )];
            if state != read {
                barriers.push(crate::host::transition(texture, read, state));
            }
            list.ResourceBarrier(&barriers);
            list.CopyBufferRegion(&self.range_back, 0, &self.range, 0, 8);
            list.ResourceBarrier(&[crate::host::transition(
                &self.range,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
                common,
            )]);
        }
        Ok(())
    }

    /// Records `lines` at the window buffer's top left (`rtv`, `size`,
    /// `format`; in the render target state), on a dark band.
    fn text(
        &mut self,
        host: &Host,
        list: &ID3D12GraphicsCommandList,
        rtv: D3D12_CPU_DESCRIPTOR_HANDLE,
        format: DXGI_FORMAT,
        size: [u32; 2],
        lines: &[String],
    ) {
        let Some([text, _]) = self.pipelines(host, format) else {
            return;
        };
        let scale = (size[1] / 540).max(1);
        let [cell_w, cell_h] = CELL.map(|c| (c * scale) as f32);
        let margin = (4 * scale) as f32;
        let longest = lines.iter().map(String::len).max().unwrap_or(0).min(LINE);
        let width = longest as f32 * cell_w + scale as f32;
        let mut constants = Constants {
            target: [size[0] as f32, size[1] as f32],
            ..Default::default()
        };
        self.bind(list, &constants, true);
        // SAFETY: an open list; the window buffer's view was just made.
        unsafe {
            target(list, rtv, size);
            list.SetPipelineState(&text);
            for (i, line) in lines.iter().enumerate() {
                let top = margin + i as f32 * cell_h;
                let (words, count) = pack(line);
                constants.rect = [margin, top, margin + width, top + cell_h];
                constants.head = [scale, count];
                constants.words = words;
                list.SetGraphicsRoot32BitConstants(
                    DRAW,
                    (size_of::<Constants>() / 4) as u32,
                    std::ptr::from_ref(&constants).cast(),
                    0,
                );
                list.DrawInstanced(4, 1, 0, 0);
            }
        }
    }

    /// After a view's list ran: the range its pass found.
    fn read_range(&mut self) {
        let mut data = std::ptr::null_mut();
        // SAFETY: the copy finished (the list was waited for); the mapping
        // holds the two keys.
        unsafe {
            if self.range_back.Map(0, None, Some(&mut data)).is_err() {
                return;
            }
            let [low, high] = data.cast::<[u32; 2]>().read();
            self.range_back.Unmap(0, None);
            self.last_range =
                (low <= high).then(|| (self.ranging.clone(), unkey(low), unkey(high)));
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

/// A buffer of `bytes` the CPU reads back, in the copy destination state.
fn readback(host: &Host, bytes: u64) -> Option<ID3D12Resource> {
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_READBACK,
        ..Default::default()
    };
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: bytes,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        ..Default::default()
    };
    let mut buffer: Option<ID3D12Resource> = None;
    // SAFETY: plain creation; the out pointer is ours.
    unsafe {
        host.device
            .CreateCommittedResource(
                &heap,
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATE_COPY_DEST,
                None,
                &mut buffer,
            )
            .ok()?;
    }
    buffer
}

/// The overlay's lines at the present of `frame`, at most `max` characters
/// long: the counters if it is on, then the view's line.
fn lines(
    frame: u64,
    line: &FrameLine,
    host: &Host,
    overlay: bool,
    view: Option<&str>,
    max: usize,
) -> Vec<String> {
    const GB: f64 = (1u64 << 30) as f64;
    let mut text = Vec::new();
    if overlay {
        let (hits, misses) = crate::shader_cache::counts();
        let [loaded, made] = crate::pipeline::library_pipelines();
        let library = match crate::pipeline::kept()[4] >> 20 {
            0 => "no library loaded".to_owned(),
            mb => format!("library of {mb} MB loaded"),
        };
        text.push(format!(
            "frame {frame}: {:.1} ms ({:.1} fps); preparing {:.1} ms, GPU {:.1} ms, copying back {:.1} ms",
            line.ms, line.fps, line.cpu, line.gpu, line.back
        ));
        text.push(format!(
            "a frame: {:.0} draws, {:.0} dispatches, {:.0} dropped without a pipeline ({} being made)",
            line.draws,
            line.dispatches,
            line.dropped,
            crate::pipeline::making()
        ));
        text.push(format!(
            "shaders: {hits} from the cache, {misses} translated; pipelines: {loaded} from the library, {made} made; {library}"
        ));
        text.push(match host.video_memory() {
            Some([budget, used, _, shared]) => format!(
                "VRAM {:.2} GB of {:.2}; {:.2} GB in system memory",
                used as f64 / GB,
                budget as f64 / GB,
                shared as f64 / GB
            ),
            None => "VRAM: not known".to_owned(),
        });
        let live = crate::live::active();
        let live = if live.is_empty() {
            "live switches: none set".to_owned()
        } else {
            let set: Vec<String> = live
                .iter()
                .map(|(name, value)| format!("{name}={value}"))
                .collect();
            format!("live switches: {}", set.join(" "))
        };
        text.extend(wrap(&live, max));
    }
    if let Some(view) = view {
        text.extend(wrap(view, max));
    }
    text
}

/// At the present of `frame`, as `at` asks: records into `list`, after the
/// frame's copy into the window buffer `back` (in the copy destination
/// state, where it is left), the view in place of the frame, the shot's
/// read back (of what the window shows without the overlay), and the
/// overlay. `made` holds the overlay's objects. What is left to do after
/// the list ran ([`finish`]), if anything.
#[allow(clippy::too_many_arguments)]
pub(crate) fn record(
    made: &mut Option<Option<Overlay>>,
    host: &Host,
    list: &HostList,
    back: &ID3D12Resource,
    frame: u64,
    line: &FrameLine,
    at: AtPresent,
) -> Option<After> {
    let mut overlay = made.get_or_insert_with(|| Overlay::new(host)).as_mut();
    let list = &list.list;
    // SAFETY: a live resource.
    let d = unsafe { back.GetDesc() };
    let (format, size) = (d.Format, [d.Width as u32, d.Height]);
    // A view of the window buffer from the host's ring, made where it is
    // recorded (the ring comes round).
    let rtv = || host.rtv(Some(back), None);
    let mut state = D3D12_RESOURCE_STATE_COPY_DEST;
    let mut go = |to: D3D12_RESOURCE_STATES| {
        if state != to {
            // SAFETY: an open list and the window's live buffer.
            unsafe { list.ResourceBarrier(&[crate::host::transition(back, state, to)]) };
            state = to;
        }
    };
    let mut after = After {
        shot: None,
        range: false,
    };
    // The view's line: what it shows and the range of values it found at
    // the last present that showed the same, or why it shows nothing.
    let mut view = None;
    match (&at.view, overlay.as_deref_mut()) {
        (Some(Ok(viewed)), Some(o)) => {
            go(D3D12_RESOURCE_STATE_RENDER_TARGET);
            view = Some(match o.view(host, list, rtv(), format, size, viewed) {
                Ok(()) => {
                    after.range = true;
                    match o
                        .last_range
                        .as_ref()
                        .filter(|(label, _, _)| *label == viewed.label)
                    {
                        Some((_, low, high)) => {
                            format!("{}: values {low:.4e} to {high:.4e}", viewed.label)
                        }
                        None => viewed.label.clone(),
                    }
                }
                Err(why) => format!("{}: {why}", viewed.label),
            });
        }
        (Some(Err(why)), _) => view = Some(why.clone()),
        _ => {}
    }
    if let Some(name) = &at.shot {
        go(D3D12_RESOURCE_STATE_COPY_SOURCE);
        after.shot = crate::capture::read_back(&host.device, list, back)
            .map(|(buffer, footprint)| (name.clone(), buffer, footprint));
    }
    if let Some(o) = overlay
        && (at.overlay || at.view.is_some())
    {
        let max = (size[0] / (CELL[0] * (size[1] / 540).max(1))).saturating_sub(2) as usize;
        let lines = lines(
            frame,
            line,
            host,
            at.overlay,
            view.as_deref(),
            max.min(LINE),
        );
        go(D3D12_RESOURCE_STATE_RENDER_TARGET);
        o.text(host, list, rtv(), format, size, &lines);
    }
    go(D3D12_RESOURCE_STATE_COPY_DEST);
    (after.shot.is_some() || after.range).then_some(after)
}

/// After the list [`record`] recorded into ran: saves the shot, reads the
/// view's range back.
pub(crate) fn finish(made: &mut Option<Option<Overlay>>, after: After) {
    if let Some((name, buffer, footprint)) = &after.shot {
        crate::capture::save_shot(name, buffer, footprint);
    }
    if after.range
        && let Some(Some(o)) = made
    {
        o.read_range();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// Formats are read as dumps show them: depth by its plane, one channel
    /// as grey from its range, light through the curve, 8-bit as it is.
    #[test]
    fn views_read_formats_as_dumps_show_them() {
        let r = |f| reading(f).unwrap();
        assert_eq!(
            r(44),
            Reading {
                format: 46,
                integers: false,
                single: Some(0),
                show: SHOW_RANGE
            }
        );
        assert_eq!(r(39).format, 41);
        assert_eq!(r(10).show, SHOW_HIGHEST | SHOW_CURVE);
        assert_eq!(
            r(27),
            Reading {
                format: 28,
                integers: false,
                single: None,
                show: 0
            }
        );
        assert!(r(42).integers);
        assert_eq!(r(65).single, Some(3));
        assert_eq!(reading(40), None, "typed depth has no shader view");
        assert_eq!(shown(r(10), Channel::Rgb), (4, SHOW_HIGHEST | SHOW_CURVE));
        assert_eq!(shown(r(10), Channel::A), (3, SHOW_RANGE | SHOW_CURVE));
        assert_eq!(shown(r(44), Channel::Rgb), (0, SHOW_RANGE));
        assert_eq!(shown(r(28), Channel::G), (1, 0));
    }

    /// The range pass's keys order as the floats do.
    #[test]
    fn range_keys_order_as_floats() {
        let key = |f: f32| {
            let u = f.to_bits();
            if u & 0x8000_0000 != 0 {
                !u
            } else {
                u | 0x8000_0000
            }
        };
        let values = [-1e30f32, -2.5, -0.0, 0.0, 1e-20, 0.5, 3.0, 1e30];
        for pair in values.windows(2) {
            assert!(key(pair[0]) <= key(pair[1]), "{pair:?}");
        }
        for v in values {
            assert_eq!(unkey(key(v)).to_bits(), v.to_bits());
        }
    }
}
