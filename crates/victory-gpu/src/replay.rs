//! A compute dispatch captured from a run (`crate::dispatch`, with
//! `VICTORY_DISPATCH_CAPTURE`) run again offline, on a device of its own:
//! `victory replay-dispatch <folder>`. The resources are made again with
//! what they held before the dispatch, the view heap's captured slots made
//! again from their descriptions, our root signature and the root
//! arguments set as the run set them; what the dispatch writes is read back
//! and compared with what it wrote in the run.
//!
//! A program that computes other values from the same inputs depending on
//! how it was compiled (a race between waves, undefined behaviour) shows
//! with `--variants N`: N programs, the captured one and N-1 translations of
//! the captured container with a different salt (`victory_shader::emit::
//! set_salt`), which the driver compiles anew, each run `--runs` times;
//! their outputs are compared with the first's. `--dxil` and
//! `--retranslate` run other code on the same inputs (a translator fix).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use victory_shader::emit::HostShader;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::Interface;

/// A JSON value: the manifest's format.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Json {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    List(Vec<Json>),
    Map(Vec<(String, Json)>),
}

impl Json {
    #[cfg(any(test, feature = "diagnostics"))]
    pub(crate) fn ints(values: &[u32]) -> Json {
        Json::List(values.iter().map(|&v| Json::Int(i64::from(v))).collect())
    }

    fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Map(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    fn int(&self) -> Option<i64> {
        match self {
            Json::Int(v) => Some(*v),
            _ => None,
        }
    }

    fn str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    fn list(&self) -> &[Json] {
        match self {
            Json::List(items) => items,
            _ => &[],
        }
    }

    /// A number written as a hex string (`"0x..."`) or as an integer.
    fn number(&self) -> Option<u64> {
        match self {
            Json::Int(v) => u64::try_from(*v).ok(),
            Json::Str(s) => u64::from_str_radix(s.strip_prefix("0x")?, 16).ok(),
            _ => None,
        }
    }

    /// Indented text, lists of plain values on one line.
    #[cfg(any(test, feature = "diagnostics"))]
    pub(crate) fn to_text(&self) -> String {
        let mut out = String::new();
        self.write(&mut out, 0);
        out.push('\n');
        out
    }

    #[cfg(any(test, feature = "diagnostics"))]
    fn write(&self, out: &mut String, indent: usize) {
        let pad = |out: &mut String, n: usize| out.extend(std::iter::repeat_n(' ', n));
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Json::Int(v) => out.push_str(&v.to_string()),
            Json::Float(v) => out.push_str(&format!("{v:?}")),
            Json::Str(s) => {
                out.push('"');
                for c in s.chars() {
                    match c {
                        '"' => out.push_str("\\\""),
                        '\\' => out.push_str("\\\\"),
                        '\n' => out.push_str("\\n"),
                        c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                        c => out.push(c),
                    }
                }
                out.push('"');
            }
            Json::List(items)
                if items
                    .iter()
                    .all(|i| !matches!(i, Json::List(_) | Json::Map(_))) =>
            {
                out.push('[');
                for (n, item) in items.iter().enumerate() {
                    if n > 0 {
                        out.push_str(", ");
                    }
                    item.write(out, indent);
                }
                out.push(']');
            }
            Json::List(items) => {
                out.push_str("[\n");
                for (n, item) in items.iter().enumerate() {
                    pad(out, indent + 2);
                    item.write(out, indent + 2);
                    out.push_str(if n + 1 < items.len() { ",\n" } else { "\n" });
                }
                pad(out, indent);
                out.push(']');
            }
            Json::Map(fields) => {
                out.push_str("{\n");
                for (n, (k, v)) in fields.iter().enumerate() {
                    pad(out, indent + 2);
                    Json::Str(k.clone()).write(out, indent + 2);
                    out.push_str(": ");
                    v.write(out, indent + 2);
                    out.push_str(if n + 1 < fields.len() { ",\n" } else { "\n" });
                }
                pad(out, indent);
                out.push('}');
            }
        }
    }
}

/// Parses JSON text.
pub(crate) fn parse(text: &str) -> Result<Json, String> {
    struct P<'a>(&'a [u8], usize);
    impl P<'_> {
        fn ws(&mut self) {
            while self.1 < self.0.len() && self.0[self.1].is_ascii_whitespace() {
                self.1 += 1;
            }
        }
        fn eat(&mut self, c: u8) -> bool {
            self.ws();
            let ok = self.0.get(self.1) == Some(&c);
            self.1 += usize::from(ok);
            ok
        }
        fn value(&mut self) -> Result<Json, String> {
            self.ws();
            let at = self.1;
            match self.0.get(at) {
                Some(b'{') => {
                    self.1 += 1;
                    let mut fields = Vec::new();
                    if self.eat(b'}') {
                        return Ok(Json::Map(fields));
                    }
                    loop {
                        self.ws();
                        let Json::Str(k) = self.value()? else {
                            return Err(format!("a key expected at {}", self.1));
                        };
                        if !self.eat(b':') {
                            return Err(format!("':' expected at {}", self.1));
                        }
                        fields.push((k, self.value()?));
                        if self.eat(b'}') {
                            return Ok(Json::Map(fields));
                        }
                        if !self.eat(b',') {
                            return Err(format!("',' or '}}' expected at {}", self.1));
                        }
                    }
                }
                Some(b'[') => {
                    self.1 += 1;
                    let mut items = Vec::new();
                    if self.eat(b']') {
                        return Ok(Json::List(items));
                    }
                    loop {
                        items.push(self.value()?);
                        if self.eat(b']') {
                            return Ok(Json::List(items));
                        }
                        if !self.eat(b',') {
                            return Err(format!("',' or ']' expected at {}", self.1));
                        }
                    }
                }
                Some(b'"') => {
                    self.1 += 1;
                    let mut s = String::new();
                    loop {
                        let rest =
                            std::str::from_utf8(&self.0[self.1..]).map_err(|e| e.to_string())?;
                        let mut chars = rest.char_indices();
                        let Some((_, c)) = chars.next() else {
                            return Err("unterminated string".into());
                        };
                        self.1 += c.len_utf8();
                        match c {
                            '"' => return Ok(Json::Str(s)),
                            '\\' => {
                                let e = *self.0.get(self.1).ok_or("unterminated escape")?;
                                self.1 += 1;
                                match e {
                                    b'n' => s.push('\n'),
                                    b't' => s.push('\t'),
                                    b'r' => s.push('\r'),
                                    b'u' => {
                                        let hex = std::str::from_utf8(
                                            self.0.get(self.1..self.1 + 4).ok_or("short \\u")?,
                                        )
                                        .map_err(|e| e.to_string())?;
                                        let code = u32::from_str_radix(hex, 16)
                                            .map_err(|e| e.to_string())?;
                                        s.push(char::from_u32(code).unwrap_or('?'));
                                        self.1 += 4;
                                    }
                                    other => s.push(char::from(other)),
                                }
                            }
                            c => s.push(c),
                        }
                    }
                }
                Some(b't') if self.0[at..].starts_with(b"true") => {
                    self.1 += 4;
                    Ok(Json::Bool(true))
                }
                Some(b'f') if self.0[at..].starts_with(b"false") => {
                    self.1 += 5;
                    Ok(Json::Bool(false))
                }
                Some(b'n') if self.0[at..].starts_with(b"null") => {
                    self.1 += 4;
                    Ok(Json::Null)
                }
                Some(c) if *c == b'-' || c.is_ascii_digit() => {
                    while self.1 < self.0.len()
                        && matches!(
                            self.0[self.1],
                            b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9'
                        )
                    {
                        self.1 += 1;
                    }
                    let text =
                        std::str::from_utf8(&self.0[at..self.1]).map_err(|e| e.to_string())?;
                    match text.parse::<i64>() {
                        Ok(v) => Ok(Json::Int(v)),
                        Err(_) => text
                            .parse::<f64>()
                            .map(Json::Float)
                            .map_err(|e| e.to_string()),
                    }
                }
                _ => Err(format!("a value expected at {at}")),
            }
        }
    }
    let mut p = P(text.as_bytes(), 0);
    let v = p.value()?;
    p.ws();
    if p.1 != p.0.len() {
        return Err(format!("text after the value at {}", p.1));
    }
    Ok(v)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

/// The bytes of a description (plain data).
#[cfg(feature = "diagnostics")]
fn bytes_of<T: Copy>(v: &T) -> Vec<u8> {
    // SAFETY: a plain-data value of `size_of::<T>()` bytes.
    unsafe { std::slice::from_raw_parts(std::ptr::from_ref(v).cast::<u8>(), size_of::<T>()) }
        .to_vec()
}

/// A description from [`bytes_of`]'s bytes.
fn from_bytes<T: Copy>(bytes: &[u8]) -> Option<T> {
    // SAFETY: exactly the bytes of a plain-data value.
    (bytes.len() == size_of::<T>()).then(|| unsafe { bytes.as_ptr().cast::<T>().read_unaligned() })
}

#[cfg(feature = "diagnostics")]
fn root_name(root: victory_shader::RootType) -> String {
    use victory_shader::RootType;
    match root {
        RootType::Constants => "constants".into(),
        RootType::Table => "table".into(),
        RootType::Descriptor => "descriptor".into(),
        RootType::Other(k) => format!("host {k}"),
        other => format!("{other:?}"),
    }
}

/// The user data the shader reads (its layout) and the values the run gave
/// it, for the manifest.
#[cfg(feature = "diagnostics")]
pub(crate) fn user_data_json(layout: &[victory_shader::emit::UserData], values: &[u32]) -> Json {
    Json::List(
        layout
            .iter()
            .map(|u| {
                Json::Map(vec![
                    ("index".into(), Json::Int(i64::from(u.index))),
                    ("slot".into(), Json::Int(i64::from(u.slot))),
                    ("root".into(), Json::Str(root_name(u.root))),
                    ("dword".into(), Json::Int(i64::from(u.dword))),
                    (
                        "value".into(),
                        Json::Str(format!(
                            "{:#x}",
                            values.get(u.index as usize).copied().unwrap_or(0)
                        )),
                    ),
                ])
            })
            .collect(),
    )
}

/// A resource description for the manifest; a buffer's width is `len`, the
/// bytes captured.
#[cfg(feature = "diagnostics")]
pub(crate) fn desc_json(d: &D3D12_RESOURCE_DESC, len: Option<u64>) -> Json {
    let n = |v: u64| Json::Int(v as i64);
    Json::Map(vec![
        ("dimension".into(), n(d.Dimension.0 as u64)),
        ("width".into(), n(len.unwrap_or(d.Width))),
        ("height".into(), n(u64::from(d.Height))),
        ("depth_or_array".into(), n(u64::from(d.DepthOrArraySize))),
        ("mips".into(), n(u64::from(d.MipLevels))),
        ("format".into(), n(d.Format.0 as u64)),
        ("samples".into(), n(u64::from(d.SampleDesc.Count))),
        ("flags".into(), n(d.Flags.0 as u64)),
    ])
}

/// A captured view slot for the manifest: its view's description with a
/// buffer view's first element counted from `base`, the captured bytes'
/// start; `offset`: a constant view's offset there.
#[cfg(feature = "diagnostics")]
pub(crate) fn slot_json(
    index: u32,
    made: Option<&crate::dispatch::Made>,
    resource: Option<usize>,
    offset: u64,
    base: u64,
) -> Json {
    use crate::dispatch::Made;
    let (kind, desc) = match made {
        None => ("unknown", None),
        Some(Made::Srv(d)) => (
            "srv",
            d.map(|mut d| {
                if d.ViewDimension == D3D12_SRV_DIMENSION_BUFFER {
                    // SAFETY: a buffer view's description holds the buffer fields.
                    unsafe {
                        let b = &mut d.Anonymous.Buffer;
                        b.FirstElement -= base
                            / crate::dispatch::element(
                                b.StructureByteStride,
                                b.Flags.0 & 1 != 0,
                                d.Format,
                            )
                            .max(1);
                    }
                }
                bytes_of(&d)
            }),
        ),
        Some(Made::Uav(d)) => (
            "uav",
            d.map(|mut d| {
                if d.ViewDimension == D3D12_UAV_DIMENSION_BUFFER {
                    // SAFETY: as above.
                    unsafe {
                        let b = &mut d.Anonymous.Buffer;
                        b.FirstElement -= base
                            / crate::dispatch::element(
                                b.StructureByteStride,
                                b.Flags.0 & 1 != 0,
                                d.Format,
                            )
                            .max(1);
                    }
                }
                bytes_of(&d)
            }),
        ),
        Some(Made::Cbv(d)) => (
            "cbv",
            Some(bytes_of(&D3D12_CONSTANT_BUFFER_VIEW_DESC {
                BufferLocation: 0,
                ..*d
            })),
        ),
        Some(Made::Sampler(d)) => ("sampler", Some(bytes_of(d))),
    };
    let mut fields = vec![
        ("slot".to_owned(), Json::Int(i64::from(index))),
        ("kind".into(), Json::Str(kind.into())),
        (
            "desc".into(),
            desc.map_or(Json::Null, |d| Json::Str(hex(&d))),
        ),
        (
            "resource".into(),
            resource.map_or(Json::Null, |r| Json::Int(r as i64)),
        ),
    ];
    if kind == "cbv" {
        fields.push(("offset".into(), Json::Int(offset as i64)));
    }
    Json::Map(fields)
}

/// What the replay runs.
pub struct Options {
    /// Programs compiled: the captured one, then translations with salts 1, 2, ...
    pub variants: u32,
    /// Runs of each program.
    pub runs: u32,
    /// Code to run instead of the captured code.
    pub dxil: Option<PathBuf>,
    /// The captured container translated by this build's translator instead.
    pub retranslate: bool,
    /// Differing positions shown per resource.
    pub show: usize,
    /// Where to write a picture of the texels of each texture output that
    /// differ from the first run's (white), and each program's first
    /// outputs (packed rows), if anywhere.
    pub diffs: Option<PathBuf>,
    /// Dispatches the groups this many at a time in x (0: as captured), each
    /// part from its first group: other co-scheduling of groups and waves,
    /// the same results for code without races.
    pub split: u32,
    /// Zeroes each input in turn and runs the first program once more: which
    /// outputs change, and how many of their bytes where the first run
    /// differs from the capture (the inputs that difference comes from).
    pub sensitivity: bool,
    /// After the runs of each program, has the driver commit its background
    /// work (it compiles shaders again after their first use) and runs it
    /// as many times again.
    pub background: bool,
}

/// A resource made again on the replay's device.
struct Made {
    id: usize,
    resource: ID3D12Resource,
    texture: bool,
    written: bool,
    /// The state the dispatch reads or writes it in.
    state: D3D12_RESOURCE_STATES,
    /// What it held before the dispatch, in an upload buffer in the copy
    /// layout (`layout`).
    upload: Option<ID3D12Resource>,
    layout: Layout,
    /// What the run wrote (packed rows), if captured.
    after: Option<Vec<u8>>,
    readback: Option<ID3D12Resource>,
    what: String,
}

/// Subresource footprints, their rows and bytes per row, and the bytes in all.
struct Layout {
    footprints: Vec<D3D12_PLACED_SUBRESOURCE_FOOTPRINT>,
    rows: Vec<u32>,
    row_bytes: Vec<u64>,
    total: u64,
}

impl Layout {
    fn of(device: &ID3D12Device, desc: &D3D12_RESOURCE_DESC) -> Layout {
        let texture = desc.Dimension != D3D12_RESOURCE_DIMENSION_BUFFER;
        let mut planes = 1;
        if texture {
            let mut info = D3D12_FEATURE_DATA_FORMAT_INFO {
                Format: desc.Format,
                PlaneCount: 1,
            };
            // SAFETY: the structure the feature asks for.
            if unsafe {
                device.CheckFeatureSupport(
                    D3D12_FEATURE_FORMAT_INFO,
                    (&raw mut info).cast(),
                    size_of_val(&info) as u32,
                )
            }
            .is_ok()
            {
                planes = u32::from(info.PlaneCount.max(1));
            }
        }
        let arrays = if desc.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D || !texture {
            1
        } else {
            u32::from(desc.DepthOrArraySize)
        };
        let n = (u32::from(desc.MipLevels.max(1)) * arrays * planes) as usize;
        let mut l = Layout {
            footprints: vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); n],
            rows: vec![0; n],
            row_bytes: vec![0; n],
            total: 0,
        };
        // SAFETY: outputs for `n` subresources.
        unsafe {
            device.GetCopyableFootprints(
                desc,
                0,
                n as u32,
                0,
                Some(l.footprints.as_mut_ptr()),
                Some(l.rows.as_mut_ptr()),
                Some(l.row_bytes.as_mut_ptr()),
                Some(&mut l.total),
            );
        }
        l
    }

    /// Bytes of the packed rows ([`Layout::pack`]).
    fn packed(&self) -> usize {
        self.footprints
            .iter()
            .enumerate()
            .map(|(i, f)| {
                f.Footprint.Depth as usize * self.rows[i] as usize * self.row_bytes[i] as usize
            })
            .sum()
    }

    /// The footprints' rows, without their padding.
    fn pack(&self, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.packed());
        self.rows_each(|row, at| out.extend_from_slice(&data[at..at + row]));
        out
    }

    /// Packed rows laid out in the footprints.
    fn unpack(&self, packed: &[u8], data: &mut [u8]) {
        let mut from = 0;
        self.rows_each(|row, at| {
            let n = row.min(packed.len().saturating_sub(from));
            data[at..at + n].copy_from_slice(&packed[from..from + n]);
            from += row;
        });
    }

    /// Each row's bytes and place in the footprints, in packed order.
    fn rows_each(&self, mut f: impl FnMut(usize, usize)) {
        for (i, fp) in self.footprints.iter().enumerate() {
            let pitch = fp.Footprint.RowPitch as usize;
            for z in 0..fp.Footprint.Depth as usize {
                for row in 0..self.rows[i] as usize {
                    f(
                        self.row_bytes[i] as usize,
                        fp.Offset as usize + (z * self.rows[i] as usize + row) * pitch,
                    );
                }
            }
        }
    }

    /// Where packed byte `at` lies: subresource, x (in elements), y, z.
    fn place(&self, at: usize) -> (usize, u64, u32, u32) {
        let mut base = 0;
        for (i, fp) in self.footprints.iter().enumerate() {
            let (rows, row) = (self.rows[i] as usize, self.row_bytes[i] as usize);
            let size = fp.Footprint.Depth as usize * rows * row;
            if at < base + size {
                let off = at - base;
                let element = (row / fp.Footprint.Width.max(1) as usize).max(1);
                return (
                    i,
                    ((off % row) / element) as u64,
                    ((off / row) % rows) as u32,
                    (off / row / rows) as u32,
                );
            }
            base += size;
        }
        (usize::MAX, 0, 0, 0)
    }
}

/// The replay's device and its queue.
struct Gpu {
    device: ID3D12Device,
    queue: ID3D12CommandQueue,
    allocator: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    fence: ID3D12Fence,
    value: u64,
}

impl Gpu {
    fn new() -> Result<Gpu> {
        // SAFETY: plain D3D12 object creation.
        unsafe {
            // `VICTORY_D3D12_DEBUG`: the debug layer, its messages printed
            // after each run (a replay set up otherwise than the run).
            if std::env::var_os("VICTORY_D3D12_DEBUG").is_some() {
                let mut layer: Option<ID3D12Debug> = None;
                if D3D12GetDebugInterface(&mut layer).is_ok()
                    && let Some(layer) = layer
                {
                    layer.EnableDebugLayer();
                }
            }
            let mut device: Option<ID3D12Device> = None;
            D3D12CreateDevice(None, D3D_FEATURE_LEVEL_12_0, &mut device)
                .context("no D3D12 device")?;
            let device = device.context("no D3D12 device")?;
            let queue: ID3D12CommandQueue =
                device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                    Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                    ..Default::default()
                })?;
            let allocator: ID3D12CommandAllocator =
                device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
            let list: ID3D12GraphicsCommandList =
                device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)?;
            let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE)?;
            Ok(Gpu {
                device,
                queue,
                allocator,
                list,
                fence,
                value: 0,
            })
        }
    }

    /// Runs what the list recorded and waits for it; the list is open again.
    fn run(&mut self) -> Result<()> {
        // SAFETY: an open list of this device; the wait blocks until the
        // fence passes (no event).
        unsafe {
            self.list.Close()?;
            self.queue.ExecuteCommandLists(&[Some(self.list.cast()?)]);
            self.value += 1;
            self.queue.Signal(&self.fence, self.value)?;
            self.fence
                .SetEventOnCompletion(self.value, windows::Win32::Foundation::HANDLE::default())?;
            self.messages();
            if let Err(e) = self.device.GetDeviceRemovedReason() {
                bail!("the replay's device was removed: {e}");
            }
            self.allocator.Reset()?;
            self.list.Reset(&self.allocator, None)?;
        }
        Ok(())
    }

    /// Prints and clears the debug layer's messages, if it is on.
    fn messages(&self) {
        let Ok(queue) = self.device.cast::<ID3D12InfoQueue>() else {
            return;
        };
        // SAFETY: each message is read into a buffer of the size the queue
        // reports, then the queue is emptied.
        unsafe {
            for i in 0..queue.GetNumStoredMessages() {
                let mut size = 0;
                if queue.GetMessage(i, None, &mut size).is_err() || size == 0 {
                    continue;
                }
                let mut buffer = vec![0u64; size.div_ceil(8)];
                let message = buffer.as_mut_ptr().cast::<D3D12_MESSAGE>();
                if queue.GetMessage(i, Some(message), &mut size).is_ok() {
                    let m = &*message;
                    let text = std::slice::from_raw_parts(m.pDescription, m.DescriptionByteLength);
                    println!("D3D12: {}", String::from_utf8_lossy(text).trim_end_matches('\0'));
                }
            }
            queue.ClearStoredMessages();
        }
    }

    fn buffer(
        &self,
        kind: D3D12_HEAP_TYPE,
        size: u64,
        flags: D3D12_RESOURCE_FLAGS,
        state: D3D12_RESOURCE_STATES,
    ) -> Result<ID3D12Resource> {
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size.max(256),
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: flags,
            ..Default::default()
        };
        self.committed(kind, &desc, state)
    }

    fn committed(
        &self,
        kind: D3D12_HEAP_TYPE,
        desc: &D3D12_RESOURCE_DESC,
        state: D3D12_RESOURCE_STATES,
    ) -> Result<ID3D12Resource> {
        let mut made: Option<ID3D12Resource> = None;
        // SAFETY: plain creation.
        unsafe {
            self.device.CreateCommittedResource(
                &D3D12_HEAP_PROPERTIES {
                    Type: kind,
                    ..Default::default()
                },
                D3D12_HEAP_FLAG_NONE,
                desc,
                state,
                None,
                &mut made,
            )?;
        }
        made.context("resource not made")
    }
}

fn read_mapped(buffer: &ID3D12Resource, len: usize) -> Result<Vec<u8>> {
    let mut data = std::ptr::null_mut();
    // SAFETY: a read-back buffer at least `len` long.
    unsafe {
        buffer.Map(0, None, Some(&mut data))?;
        let bytes = std::slice::from_raw_parts(data.cast::<u8>(), len).to_vec();
        buffer.Unmap(0, None);
        Ok(bytes)
    }
}

fn transition(
    resource: &ID3D12Resource,
    before: D3D12_RESOURCE_STATES,
    after: D3D12_RESOURCE_STATES,
) -> D3D12_RESOURCE_BARRIER {
    crate::host::transition(resource, before, after)
}

fn uav_barrier() -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                pResource: std::mem::ManuallyDrop::new(None),
            }),
        },
    }
}

/// The binding layout a program must share with the capture to run on its
/// root signature and arguments.
fn same_layout(a: &HostShader, b: &HostShader) -> bool {
    a.user_data == b.user_data
        && a.root_cbvs == b.root_cbvs
        && a.root_srvs == b.root_srvs
        && a.root_uavs == b.root_uavs
        && a.static_samplers == b.static_samplers
}

/// What a replay found.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Summary {
    /// Runs, of all programs.
    pub runs: usize,
    /// Runs whose outputs differ from what the dispatch wrote in the run.
    pub unlike_capture: usize,
    /// Runs whose outputs differ from the first run's.
    pub unlike_first: usize,
}

/// Runs the dispatch captured in `dir` as `o` asks, and prints what it found.
pub fn replay_dispatch(dir: &Path, o: &Options) -> Result<Summary> {
    let read = |name: &str| {
        std::fs::read(dir.join(name))
            .with_context(|| format!("reading {}", dir.join(name).display()))
    };
    let manifest =
        parse(&String::from_utf8_lossy(&read("manifest.json")?)).map_err(anyhow::Error::msg)?;
    let captured = crate::shader_cache::decode(&read("host-shader.bin")?)
        .context("host-shader.bin does not decode")?;
    let container = read("shader.cdxil");
    let field = |k: &str| {
        manifest
            .get(k)
            .with_context(|| format!("manifest without {k}"))
    };

    // The translator as the run had it, set when something is translated.
    let translation = field("translation")?;
    let setting = |k: &str| translation.get(k).and_then(Json::int).unwrap_or(0);
    let settings = std::sync::Once::new();
    let translate = |salt: u32| -> Result<HostShader> {
        let container = container.as_ref().map_err(|e| anyhow::anyhow!("{e:#}"))?;
        settings.call_once(|| {
            victory_shader::emit::set_view_heap_limit(setting("view_heap_limit") as u32);
            victory_shader::emit::set_host_max_wave(setting("host_max_wave") as u32);
            victory_shader::emit::set_memory_pages(translation.get("memory_pages") == Some(&Json::Bool(true)));
            for (name, here) in [
                ("group_sync", i64::from(victory_shader::emit::group_sync())),
                ("loop_limit", i64::from(victory_shader::emit::loop_limit())),
            ] {
                if setting(name) != here {
                    println!("note: the run translated with {name} {}, this process with {here} (see the VICTORY_ variables)", setting(name));
                }
            }
        });
        victory_shader::emit::set_salt(salt);
        let t = victory_shader::emit::translate(container);
        victory_shader::emit::set_salt(0);
        let t = t.map_err(|e| anyhow::anyhow!("translation: {e}"))?;
        if !same_layout(&t, &captured) {
            bail!(
                "the translation binds otherwise than the capture (user data, root views or samplers): not supported"
            );
        }
        Ok(t)
    };

    // The programs.
    let mut programs: Vec<(String, Vec<u8>)> = Vec::new();
    if let Some(path) = &o.dxil {
        programs.push((
            format!("{}", path.display()),
            std::fs::read(path).with_context(|| format!("reading {}", path.display()))?,
        ));
    } else if o.retranslate {
        let t = translate(0)?;
        let same = t.dxil == captured.dxil;
        println!(
            "translated again: {} bytes, {} the captured code",
            t.dxil.len(),
            if same { "the same as" } else { "OTHER than" }
        );
        programs.push(("translated again".into(), t.dxil));
    } else {
        programs.push(("captured".into(), captured.dxil.clone()));
        // The salted ones differ from the captured code by their salt only
        // if this build translates as the run's did.
        if o.variants > 1 && translate(0)?.dxil != captured.dxil {
            println!("note: this build translates the shader otherwise than the run did: the salted programs differ by more than their salt");
        }
    }
    for salt in 1..o.variants.max(1) {
        programs.push((format!("salt {salt}"), translate(salt)?.dxil));
    }

    let mut gpu = Gpu::new()?;
    let device = gpu.device.clone();
    // SAFETY: the blob a root signature was serialized to.
    let root: ID3D12RootSignature =
        unsafe { device.CreateRootSignature(0, &read("root-signature.bin")?) }
            .context("root signature")?;

    // The resources, with what they held before the dispatch.
    let mut made: Vec<Made> = Vec::new();
    for r in field("resources")?.list() {
        let id = r.get("id").and_then(Json::int).context("resource id")? as usize;
        let d = r.get("desc").context("resource desc")?;
        let n = |k: &str| d.get(k).and_then(Json::int).unwrap_or(0);
        let texture = n("dimension") != D3D12_RESOURCE_DIMENSION_BUFFER.0 as i64;
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION(n("dimension") as i32),
            Alignment: 0,
            Width: if texture {
                n("width") as u64
            } else {
                (n("width") as u64).next_multiple_of(256).max(256)
            },
            Height: n("height") as u32,
            DepthOrArraySize: n("depth_or_array") as u16,
            MipLevels: n("mips") as u16,
            Format: DXGI_FORMAT(n("format") as i32),
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: n("samples").max(1) as u32,
                Quality: 0,
            },
            Layout: if texture {
                D3D12_TEXTURE_LAYOUT_UNKNOWN
            } else {
                D3D12_TEXTURE_LAYOUT_ROW_MAJOR
            },
            Flags: D3D12_RESOURCE_FLAGS(n("flags") as i32),
        };
        let written = r.get("written") == Some(&Json::Bool(true));
        let state = match (written, texture) {
            (true, _) => D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            (false, true) => D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
            (false, false) => {
                D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE
                    | D3D12_RESOURCE_STATE_VERTEX_AND_CONSTANT_BUFFER
            }
        };
        let resource = gpu
            .committed(
                D3D12_HEAP_TYPE_DEFAULT,
                &desc,
                if texture {
                    D3D12_RESOURCE_STATE_COPY_DEST
                } else {
                    D3D12_RESOURCE_STATE_COMMON
                },
            )
            .with_context(|| {
                format!(
                    "resource {id} ({}x{} format {})",
                    desc.Width, desc.Height, desc.Format.0
                )
            })?;
        let layout = Layout::of(&device, &desc);
        let file = |k: &str| r.get(k).and_then(Json::str).map(|f| read(f)).transpose();
        let upload = match file("before")? {
            Some(before) => {
                let upload = gpu.buffer(
                    D3D12_HEAP_TYPE_UPLOAD,
                    layout.total,
                    D3D12_RESOURCE_FLAG_NONE,
                    D3D12_RESOURCE_STATE_GENERIC_READ,
                )?;
                let mut data = std::ptr::null_mut();
                // SAFETY: an upload buffer of `layout.total` bytes.
                unsafe {
                    upload.Map(0, None, Some(&mut data))?;
                    let to =
                        std::slice::from_raw_parts_mut(data.cast::<u8>(), layout.total as usize);
                    if texture {
                        layout.unpack(&before, to);
                    } else {
                        let n = before.len().min(to.len());
                        to[..n].copy_from_slice(&before[..n]);
                    }
                    upload.Unmap(0, None);
                }
                Some(upload)
            }
            None => None,
        };
        let readback = if written {
            Some(gpu.buffer(
                D3D12_HEAP_TYPE_READBACK,
                layout.total,
                D3D12_RESOURCE_FLAG_NONE,
                D3D12_RESOURCE_STATE_COPY_DEST,
            )?)
        } else {
            None
        };
        let after = file("after")?;
        let what = if texture {
            format!(
                "r{id} texture {}x{}x{} f{}",
                desc.Width, desc.Height, desc.DepthOrArraySize, desc.Format.0
            )
        } else {
            format!("r{id} buffer {:#x} bytes", n("width"))
        };
        made.push(Made {
            id,
            resource,
            texture,
            written,
            state,
            upload,
            layout,
            after,
            readback,
            what,
        });
    }
    let by_id = |id: usize| made.iter().find(|m| m.id == id);

    // The view heap, every slot null but the ones captured.
    let heap_slots = field("heap")?.get("slots").and_then(Json::int).unwrap_or(0) as u32;
    let slots = field("slots")?.list();
    let highest = slots
        .iter()
        .filter_map(|s| s.get("slot").and_then(Json::int))
        .max()
        .unwrap_or(0) as u32;
    // Past the null slot a capped heap keeps at its end, which the shader
    // reads for any index from there on.
    let count = heap_slots.max(highest + 1).max(setting("view_heap_limit") as u32 + 1);
    // SAFETY: plain heap creation and descriptor writes into it.
    let heap: ID3D12DescriptorHeap = unsafe {
        device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
            Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
            NumDescriptors: count,
            Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
            NodeMask: 0,
        })
    }
    .context("view heap")?;
    let step = u64::from(unsafe {
        device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV)
    });
    // SAFETY: a live heap.
    let (cpu, gpu_start) = unsafe {
        (
            heap.GetCPUDescriptorHandleForHeapStart().ptr,
            heap.GetGPUDescriptorHandleForHeapStart().ptr,
        )
    };
    let at = |slot: u32| D3D12_CPU_DESCRIPTOR_HANDLE {
        ptr: cpu + (u64::from(slot) * step) as usize,
    };
    fill_null(&device, cpu, step, count)?;
    let mut skipped = 0;
    for s in slots {
        let slot = s.get("slot").and_then(Json::int).context("slot")? as u32;
        let desc = s.get("desc").and_then(Json::str).and_then(unhex);
        let resource = s
            .get("resource")
            .and_then(Json::int)
            .and_then(|r| by_id(r as usize))
            .map(|m| &m.resource);
        // SAFETY: slots of the heap; descriptions as the run made them, of
        // resources made like the run's.
        unsafe {
            match s.get("kind").and_then(Json::str) {
                Some("srv") => {
                    let d = desc
                        .as_deref()
                        .and_then(from_bytes::<D3D12_SHADER_RESOURCE_VIEW_DESC>);
                    device.CreateShaderResourceView(
                        resource,
                        d.as_ref().map(std::ptr::from_ref),
                        at(slot),
                    );
                }
                Some("uav") => {
                    let d = desc
                        .as_deref()
                        .and_then(from_bytes::<D3D12_UNORDERED_ACCESS_VIEW_DESC>);
                    device.CreateUnorderedAccessView(
                        resource,
                        None::<&ID3D12Resource>,
                        d.as_ref().map(std::ptr::from_ref),
                        at(slot),
                    );
                }
                Some("cbv") => {
                    let mut d = desc
                        .as_deref()
                        .and_then(from_bytes::<D3D12_CONSTANT_BUFFER_VIEW_DESC>)
                        .unwrap_or_default();
                    let offset = s.get("offset").and_then(Json::number).unwrap_or(0);
                    d.BufferLocation = resource.map_or(0, |r| r.GetGPUVirtualAddress() + offset);
                    device.CreateConstantBufferView(Some(&d), at(slot));
                }
                _ => skipped += 1,
            }
        }
    }

    // The root arguments.
    let group_base = manifest
        .get("group_base")
        .map(Json::list)
        .filter(|l| l.len() == 2)
        .map(|l| {
            (
                l[0].int().unwrap_or(0) as u32,
                l[1].int().unwrap_or(0) as u32,
            )
        });
    let groups: Vec<[u32; 4]> = field("groups")?
        .list()
        .iter()
        .map(|g| {
            let v: Vec<u32> = g
                .list()
                .iter()
                .map(|x| x.int().unwrap_or(0) as u32)
                .collect();
            [
                v.first().copied().unwrap_or(0),
                v.get(1).copied().unwrap_or(1),
                v.get(2).copied().unwrap_or(1),
                v.get(3).copied().unwrap_or(0),
            ]
        })
        .collect();
    // `split`: the groups in x a few at a time, each part from its first
    // group (the program adds it to its group number).
    let groups: Vec<[u32; 4]> = match (o.split, group_base) {
        (0, _) => groups,
        (_, None) => bail!("--split needs a program that takes its first group (USER_GROUP_BASE)"),
        (n, Some(_)) => groups
            .iter()
            .flat_map(|g| (0..g[0]).step_by(n as usize).map(move |i| [n.min(g[0] - i), g[1], g[2], g[3] + i]))
            .collect(),
    };
    let zeros = gpu.buffer(
        D3D12_HEAP_TYPE_DEFAULT,
        1 << 16,
        D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        D3D12_RESOURCE_STATE_COMMON,
    )?;
    let roots = field("roots")?.list().to_vec();
    let bind = |list: &ID3D12GraphicsCommandList| -> Result<()> {
        for r in &roots {
            let param = r.get("param").and_then(Json::int).context("root param")? as u32;
            // SAFETY: an open list; the root signature has the parameter.
            unsafe {
                if let Some(values) = r.get("constants") {
                    let v: Vec<u32> = values
                        .list()
                        .iter()
                        .map(|x| x.int().unwrap_or(0) as u32)
                        .collect();
                    list.SetComputeRoot32BitConstants(param, v.len() as u32, v.as_ptr().cast(), 0);
                } else if let Some(slot) = r.get("slot").and_then(Json::int) {
                    let slot = slot as u64;
                    list.SetComputeRootDescriptorTable(
                        param,
                        D3D12_GPU_DESCRIPTOR_HANDLE {
                            ptr: gpu_start + slot * step,
                        },
                    );
                } else if let Some(kind) = r.get("view").and_then(Json::str) {
                    let address = match r
                        .get("resource")
                        .and_then(Json::int)
                        .and_then(|id| by_id(id as usize))
                    {
                        Some(m) => {
                            m.resource.GetGPUVirtualAddress()
                                + r.get("offset").and_then(Json::number).unwrap_or(0)
                        }
                        None => zeros.GetGPUVirtualAddress(),
                    };
                    match kind {
                        "cbv" => list.SetComputeRootConstantBufferView(param, address),
                        "srv" => list.SetComputeRootShaderResourceView(param, address),
                        _ => list.SetComputeRootUnorderedAccessView(param, address),
                    }
                }
            }
        }
        Ok(())
    };

    // What is captured, in short.
    let shader = manifest.get("shader").and_then(Json::str).unwrap_or("?");
    let frame = manifest.get("frame").and_then(Json::int).unwrap_or(0);
    let group_text: Vec<String> = groups
        .iter()
        .map(|g| {
            format!(
                "{}x{}x{}{}",
                g[0],
                g[1],
                g[2],
                if g[3] > 0 {
                    format!(" from {}", g[3])
                } else {
                    String::new()
                }
            )
        })
        .collect();
    println!(
        "dispatch of {shader} from frame {frame}: groups {}; {} resources ({} written), {} slots",
        group_text.join(", "),
        made.len(),
        made.iter().filter(|m| m.written).count(),
        slots.len()
    );
    if skipped > 0 {
        println!("note: {skipped} slots without a view noted are null");
    }
    for note in manifest.get("notes").map(Json::list).unwrap_or(&[]) {
        if let Some(n) = note.str() {
            println!("note: {n}");
        }
    }

    // Each program, each run.
    let mut reference: Option<(String, Vec<Vec<u8>>)> = None;
    let mut cached: Option<Vec<u8>> = None;
    let mut first_pso: Option<ID3D12PipelineState> = None;
    let mut summary = Summary::default();
    for (name, dxil) in &programs {
        let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
            // SAFETY: a borrowed pointer the description never releases.
            pRootSignature: unsafe { std::mem::transmute_copy(&root) },
            CS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: dxil.as_ptr().cast(),
                BytecodeLength: dxil.len(),
            },
            ..Default::default()
        };
        // SAFETY: the description points at live data.
        let pso: ID3D12PipelineState = unsafe { device.CreateComputePipelineState(&desc) }
            .with_context(|| format!("pipeline of {name}"))?;
        first_pso.get_or_insert_with(|| pso.clone());
        // What the driver made of it: its cached blob, against the first's.
        // SAFETY: a live pipeline; the blob is read in full before release.
        let blob = unsafe { pso.GetCachedBlob() }
            .map(|b| {
                unsafe {
                    std::slice::from_raw_parts(b.GetBufferPointer().cast::<u8>(), b.GetBufferSize())
                }
                .to_vec()
            })
            .unwrap_or_default();
        let compiled = match &cached {
            None => format!("driver blob {} bytes", blob.len()),
            Some(first) => {
                let diff = first.iter().zip(&blob).filter(|(a, b)| a != b).count()
                    + first.len().abs_diff(blob.len());
                format!(
                    "driver blob {} bytes, {diff} bytes other than the first program's",
                    blob.len()
                )
            }
        };
        if cached.is_none() {
            cached = Some(blob);
        }
        println!("program {name}: {} bytes of DXIL, {compiled}", dxil.len());
        // With `background`, the runs once more after the driver did its
        // background work on the pipeline (it compiles shaders again later).
        let passes = if o.background { 2 } else { 1 };
        for run in 0..o.runs.max(1) * passes {
            let after = run >= o.runs.max(1);
            if after && run == o.runs.max(1) {
                println!("  the driver's background work: {}", background_work(&device));
            }
            let outputs = run_once(
                &mut gpu, &made, &heap, &root, &pso, &bind, &groups, group_base,
            )?;
            let label = if after {
                format!("{name} after the background work run {}", run - o.runs.max(1))
            } else {
                format!("{name} run {run}")
            };
            summary.runs += 1;
            // The first run's outputs beside the pictures, as packed rows.
            if let Some(dir) = o.diffs.as_ref().filter(|_| run == 0) {
                for (m, out) in made.iter().filter(|m| m.written).zip(&outputs) {
                    let name = format!("{label} r{}.bin", m.id).replace(' ', "-");
                    std::fs::write(dir.join(name), out)?;
                }
            }
            // Against what the run wrote.
            let mut line = Vec::new();
            let mut unlike = false;
            for (m, out) in made.iter().filter(|m| m.written).zip(&outputs) {
                match &m.after {
                    Some(after) => {
                        unlike |= after != out;
                        line.push(compare(m, after, out, o.show, None));
                    }
                    None => line.push(format!("{}: no capture to compare", m.what)),
                }
            }
            summary.unlike_capture += usize::from(unlike);
            println!("  {label} vs the capture: {}", line.join("; "));
            match &reference {
                None => reference = Some((label, outputs)),
                Some((first, theirs)) => {
                    let diffs: Vec<String> = made
                        .iter()
                        .filter(|m| m.written)
                        .zip(theirs.iter().zip(&outputs))
                        .filter(|(_, (a, b))| a != b)
                        .map(|(m, (a, b))| {
                            let mask = o.diffs.as_ref().map(|dir| {
                                let name = format!("{label} vs {first} r{}.png", m.id);
                                dir.join(name.replace(' ', "-"))
                            });
                            compare(m, a, b, o.show, mask.as_deref())
                        })
                        .collect();
                    if diffs.is_empty() {
                        println!("  {label} vs {first}: the same");
                    } else {
                        summary.unlike_first += 1;
                        println!("  {label} vs {first}: DIFFERENT: {}", diffs.join("; "));
                    }
                }
            }
        }
    }
    if o.sensitivity {
        sensitivity(&mut gpu, &made, &heap, &root, first_pso.as_ref(), &bind, &groups, group_base, reference.as_ref().map(|r| &r.1))?;
    }
    let total = summary.runs;
    if total > 1 {
        if summary.unlike_first == 0 {
            println!("all {total} runs wrote the same");
        } else {
            println!(
                "{} of {total} runs wrote otherwise than the first",
                summary.unlike_first
            );
        }
    }
    println!(
        "{} of {total} runs wrote otherwise than the dispatch did in the run",
        summary.unlike_capture
    );
    Ok(summary)
}

/// [`Options::sensitivity`]: each input zeroed in turn, and what changes
/// against `reference` (the first run's outputs).
#[allow(clippy::too_many_arguments)]
fn sensitivity(
    gpu: &mut Gpu,
    made: &[Made],
    heap: &ID3D12DescriptorHeap,
    root: &ID3D12RootSignature,
    pso: Option<&ID3D12PipelineState>,
    bind: &dyn Fn(&ID3D12GraphicsCommandList) -> Result<()>,
    groups: &[[u32; 4]],
    group_base: Option<(u32, u32)>,
    reference: Option<&Vec<Vec<u8>>>,
) -> Result<()> {
    let (Some(pso), Some(reference)) = (pso, reference) else {
        return Ok(());
    };
    let written: Vec<&Made> = made.iter().filter(|m| m.written).collect();
    println!("inputs zeroed one at a time (bytes of each output that change; of them, where the first run differs from the capture):");
    for m in made.iter().filter(|m| !m.written) {
        let Some(upload) = &m.upload else { continue };
        let len = m.layout.total as usize;
        let mut data = std::ptr::null_mut();
        // SAFETY: an upload buffer of `len` bytes; its bytes are put back
        // after the run.
        let kept = unsafe {
            upload.Map(0, None, Some(&mut data))?;
            let bytes = std::slice::from_raw_parts_mut(data.cast::<u8>(), len);
            let kept = bytes.to_vec();
            bytes.fill(0);
            upload.Unmap(0, None);
            kept
        };
        let outputs = run_once(gpu, made, heap, root, pso, bind, groups, group_base);
        // SAFETY: as above.
        unsafe {
            upload.Map(0, None, Some(&mut data))?;
            std::slice::from_raw_parts_mut(data.cast::<u8>(), len).copy_from_slice(&kept);
            upload.Unmap(0, None);
        }
        let outputs = outputs?;
        let mut line = Vec::new();
        for ((w, out), first) in written.iter().zip(&outputs).zip(reference) {
            let n = out.len().min(first.len());
            let changed: Vec<usize> = (0..n).filter(|&i| out[i] != first[i]).collect();
            if changed.is_empty() {
                continue;
            }
            let unlike = w.after.as_ref().map_or(0, |after| {
                changed.iter().filter(|&&i| after.get(i).is_some_and(|a| *a != first[i])).count()
            });
            line.push(format!("r{} {} ({unlike})", w.id, changed.len()));
        }
        if !line.is_empty() {
            println!("  {}: {}", m.what, line.join(", "));
        }
    }
    Ok(())
}

/// Asks the driver to commit its background work now (the shaders it
/// compiles again from what it measured) and waits for it: what it says.
fn background_work(device: &ID3D12Device) -> String {
    let Ok(device) = device.cast::<ID3D12Device6>() else {
        return "no ID3D12Device6".into();
    };
    // SAFETY: an event of ours, waited for and closed; a live device.
    unsafe {
        let Ok(event) = windows::Win32::System::Threading::CreateEventW(None, false, false, None) else {
            return "no event".into();
        };
        let mut further = windows::core::BOOL::default();
        let asked = device.SetBackgroundProcessingMode(
            D3D12_BACKGROUND_PROCESSING_MODE_ALLOWED,
            D3D12_MEASUREMENTS_ACTION_COMMIT_RESULTS_HIGH_PRIORITY,
            Some(event),
            Some(&mut further),
        );
        let text = match asked {
            Ok(()) => {
                let waited = windows::Win32::System::Threading::WaitForSingleObject(event, 60_000);
                format!(
                    "committed ({}; further measurements wanted: {})",
                    if waited.0 == 0 { "done" } else { "not done in 60 s" },
                    further.as_bool()
                )
            }
            Err(e) => format!("refused: {e}"),
        };
        let _ = windows::Win32::Foundation::CloseHandle(event);
        text
    }
}

/// One run: the inputs as captured, the dispatch, the outputs read back
/// (packed rows, per written resource in order).
#[allow(clippy::too_many_arguments)]
fn run_once(
    gpu: &mut Gpu,
    made: &[Made],
    heap: &ID3D12DescriptorHeap,
    root: &ID3D12RootSignature,
    pso: &ID3D12PipelineState,
    bind: &dyn Fn(&ID3D12GraphicsCommandList) -> Result<()>,
    groups: &[[u32; 4]],
    group_base: Option<(u32, u32)>,
) -> Result<Vec<Vec<u8>>> {
    let list = gpu.list.clone();
    // SAFETY: an open list; live resources in the states noted (buffers
    // decay to common between runs, textures are left in the copy state).
    unsafe {
        for m in made {
            let Some(upload) = &m.upload else { continue };
            if m.texture {
                for (i, f) in m.layout.footprints.iter().enumerate() {
                    let to = D3D12_TEXTURE_COPY_LOCATION {
                        pResource: std::mem::transmute_copy(&m.resource),
                        Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                            SubresourceIndex: i as u32,
                        },
                    };
                    let from = D3D12_TEXTURE_COPY_LOCATION {
                        pResource: std::mem::transmute_copy(upload),
                        Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                            PlacedFootprint: *f,
                        },
                    };
                    list.CopyTextureRegion(&to, 0, 0, 0, &from, None);
                }
            } else {
                list.CopyBufferRegion(&m.resource, 0, upload, 0, m.layout.total);
            }
        }
        let into: Vec<_> = made
            .iter()
            .map(|m| transition(&m.resource, D3D12_RESOURCE_STATE_COPY_DEST, m.state))
            .collect();
        list.ResourceBarrier(&into);
        list.SetDescriptorHeaps(&[Some(heap.clone())]);
        list.SetComputeRootSignature(root);
        list.SetPipelineState(pso);
        bind(&list)?;
        for g in groups {
            if let Some((param, index)) = group_base {
                list.SetComputeRoot32BitConstant(param, g[3], index);
            }
            list.Dispatch(g[0], g[1], g[2]);
        }
        list.ResourceBarrier(&[uav_barrier()]);
        let out: Vec<_> = made
            .iter()
            .map(|m| {
                transition(
                    &m.resource,
                    m.state,
                    if m.written {
                        D3D12_RESOURCE_STATE_COPY_SOURCE
                    } else {
                        D3D12_RESOURCE_STATE_COPY_DEST
                    },
                )
            })
            .collect();
        list.ResourceBarrier(&out);
        for m in made.iter().filter(|m| m.written) {
            let Some(back) = &m.readback else { continue };
            if m.texture {
                for (i, f) in m.layout.footprints.iter().enumerate() {
                    let to = D3D12_TEXTURE_COPY_LOCATION {
                        pResource: std::mem::transmute_copy(back),
                        Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                            PlacedFootprint: *f,
                        },
                    };
                    let from = D3D12_TEXTURE_COPY_LOCATION {
                        pResource: std::mem::transmute_copy(&m.resource),
                        Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                            SubresourceIndex: i as u32,
                        },
                    };
                    list.CopyTextureRegion(&to, 0, 0, 0, &from, None);
                }
            } else {
                list.CopyBufferRegion(back, 0, &m.resource, 0, m.layout.total);
            }
        }
        let back: Vec<_> = made
            .iter()
            .filter(|m| m.written)
            .map(|m| {
                transition(
                    &m.resource,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                )
            })
            .collect();
        if !back.is_empty() {
            list.ResourceBarrier(&back);
        }
    }
    gpu.run()?;
    let mut outputs = Vec::new();
    for m in made.iter().filter(|m| m.written) {
        let Some(back) = &m.readback else { continue };
        let bytes = read_mapped(back, m.layout.total as usize)?;
        outputs.push(if m.texture {
            m.layout.pack(&bytes)
        } else {
            bytes[..m
                .after
                .as_ref()
                .map_or(bytes.len(), Vec::len)
                .min(bytes.len())]
                .to_vec()
        });
    }
    Ok(outputs)
}

/// Makes slots `0..count` of the heap at `cpu` null shader views: one in a
/// heap the CPU sees, copied over.
fn fill_null(device: &ID3D12Device, cpu: usize, step: u64, count: u32) -> Result<()> {
    const BLOCK: u32 = 1 << 16;
    let kind = D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV;
    // SAFETY: plain heap creation and copies inside live heaps.
    unsafe {
        let block: ID3D12DescriptorHeap =
            device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: kind,
                NumDescriptors: BLOCK,
                ..Default::default()
            })?;
        let start = block.GetCPUDescriptorHandleForHeapStart();
        let desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
            Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture2D: D3D12_TEX2D_SRV {
                    MipLevels: 1,
                    ..Default::default()
                },
            },
        };
        device.CreateShaderResourceView(None::<&ID3D12Resource>, Some(&desc), start);
        let at = |base: usize, i: u32| D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: base + (u64::from(i) * step) as usize,
        };
        let mut filled = 1;
        while filled < BLOCK {
            let n = filled.min(BLOCK - filled);
            device.CopyDescriptorsSimple(n, at(start.ptr, filled), start, kind);
            filled += n;
        }
        let mut done = 0;
        while done < count {
            let n = BLOCK.min(count - done);
            device.CopyDescriptorsSimple(n, at(cpu, done), start, kind);
            done += n;
        }
    }
    Ok(())
}

/// How `b` differs from `a` (packed rows of `m`): bytes, and for a texture
/// texels, with the first `show` places.
fn compare(m: &Made, a: &[u8], b: &[u8], show: usize, mask: Option<&Path>) -> String {
    if a == b {
        return format!("{}: the same", m.what);
    }
    let n = a.len().min(b.len());
    let bytes = (0..n).filter(|&i| a[i] != b[i]).count() + a.len().abs_diff(b.len());
    let mut text = format!("{}: {bytes} of {} bytes differ", m.what, a.len());
    if m.texture {
        // Texels: the element of each differing byte, counted once.
        let mut texels = 0;
        let mut last = None;
        let mut places = Vec::new();
        // The first subresource's differing texels: where in 16x16 tiles
        // (a group of the tiled passes) they lie, and the picture.
        let first = m.layout.footprints.first().map(|f| f.Footprint);
        let (w, h) = first.map_or((0, 0), |f| (f.Width as usize, m.layout.rows[0] as usize));
        let mut picture = vec![0u8; w * h];
        let (mut rows, mut columns) = ([0u32; 16], [0u32; 16]);
        let mut tiles = std::collections::BTreeSet::new();
        for i in (0..n).filter(|&i| a[i] != b[i]) {
            let (sub, x, y, z) = m.layout.place(i);
            if last == Some((sub, x, y, z)) {
                continue;
            }
            last = Some((sub, x, y, z));
            texels += 1;
            if sub == 0 && z == 0 && (x as usize) < w && (y as usize) < h {
                picture[y as usize * w + x as usize] = 255;
                rows[y as usize % 16] += 1;
                columns[x as usize % 16] += 1;
                tiles.insert((x / 16, y / 16));
            }
            if places.len() < show {
                let fp = m.layout.footprints.get(sub).map(|f| f.Footprint);
                let size = fp.map_or(4, |f| {
                    (m.layout.row_bytes[sub] / u64::from(f.Width.max(1))).max(1)
                }) as usize;
                let start = i - (i % size).min(i);
                let word = |v: &[u8]| hex(&v[start..(start + size).min(v.len())]);
                places.push(format!(
                    "sub {sub} ({x},{y},{z}) {} vs {}",
                    word(a),
                    word(b)
                ));
            }
        }
        text.push_str(&format!(" ({texels} texels; {})", places.join(", ")));
        if !tiles.is_empty() {
            text.push_str(&format!(
                "; in {} 16x16 tiles, by row in the tile {rows:?}, by column {columns:?}",
                tiles.len()
            ));
        }
        if let Some(path) = mask {
            match save_grey(path, w as u32, h as u32, &picture) {
                Ok(()) => text.push_str(&format!("; picture {}", path.display())),
                Err(e) => text.push_str(&format!("; picture not written: {e}")),
            }
        }
    } else {
        let places: Vec<String> = (0..n)
            .step_by(4)
            .filter(|&i| a[i..(i + 4).min(n)] != b[i..(i + 4).min(n)])
            .take(show)
            .map(|i| {
                format!(
                    "{i:#x} {} vs {}",
                    hex(&a[i..(i + 4).min(n)]),
                    hex(&b[i..(i + 4).min(n)])
                )
            })
            .collect();
        text.push_str(&format!(" ({})", places.join(", ")));
    }
    text
}

/// Writes `pixels` (8-bit grey, `w` x `h`) as a PNG file.
fn save_grey(path: &Path, w: u32, h: u32, pixels: &[u8]) -> std::io::Result<()> {
    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &b in bytes {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xedb8_8320 & (crc & 1).wrapping_neg());
            }
        }
        !crc
    }
    let chunk = |png: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]| {
        png.extend((data.len() as u32).to_be_bytes());
        let start = png.len();
        png.extend(kind);
        png.extend(data);
        let crc = crc32(&png[start..]);
        png.extend(crc.to_be_bytes());
    };
    let mut raw = Vec::with_capacity((w as usize + 1) * h as usize);
    for row in pixels.chunks(w.max(1) as usize).take(h as usize) {
        raw.push(0);
        raw.extend_from_slice(row);
    }
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut header = Vec::new();
    header.extend(w.to_be_bytes());
    header.extend(h.to_be_bytes());
    header.extend([8, 0, 0, 0, 0]); // 8-bit grey
    chunk(&mut png, b"IHDR", &header);
    chunk(&mut png, b"IDAT", &miniz_oxide::deflate::compress_to_vec_zlib(&raw, 3));
    chunk(&mut png, b"IEND", &[]);
    std::fs::write(path, png)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_reads_back_what_it_writes() {
        let v = Json::Map(vec![
            ("a".into(), Json::ints(&[1, 2, 3])),
            ("b".into(), Json::Str("x\"y\\z\n".into())),
            (
                "c".into(),
                Json::List(vec![
                    Json::Map(vec![("d".into(), Json::Null)]),
                    Json::Bool(false),
                ]),
            ),
            ("e".into(), Json::Int(-7)),
            ("f".into(), Json::Float(0.5)),
        ]);
        assert_eq!(parse(&v.to_text()), Ok(v));
    }

    #[test]
    fn hex_numbers_and_bytes_round_trip() {
        assert_eq!(Json::Str("0x1f".into()).number(), Some(31));
        assert_eq!(unhex(&hex(&[0, 1, 254, 255])), Some(vec![0, 1, 254, 255]));
    }

    /// A capture made here, of a shader that picks a texture and a buffer
    /// from the heap by indices in its root constants, replays to what it
    /// says the dispatch wrote, and not to other bytes.
    #[test]
    fn a_capture_replays_to_its_outputs() {
        let Some(dxc) = std::env::var_os("VICTORY_PC_DXC")
            .map(PathBuf::from)
            .or_else(|| {
                Some(
                    victory_shader::emit::validator::Validator::default_path()?
                        .with_file_name("dxc.exe"),
                )
            })
            .filter(|p| p.exists())
        else {
            eprintln!("no PC dxc.exe; skipped");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let path = |n: &str| dir.path().join(n);
        let hlsl = r#"
            #define RS "RootFlags(CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED), RootConstants(num32BitConstants=4, b0)"
            cbuffer Roots : register(b0) { uint4 u; };
            [RootSignature(RS)] [numthreads(8, 1, 1)]
            void main(uint3 id : SV_DispatchThreadID) {
                Texture2D<float> t = ResourceDescriptorHeap[u.x];
                RWByteAddressBuffer o = ResourceDescriptorHeap[u.y];
                o.Store(id.x * 4, asuint(t.Load(int3(id.x, 0, 0))) + u.z);
            }"#;
        std::fs::write(path("cs.hlsl"), hlsl).unwrap();
        let status = std::process::Command::new(&dxc)
            .args(["-nologo", "-T", "cs_6_6", "-E", "main", "-Fo"])
            .arg(path("shader.dxil"))
            .arg(path("cs.hlsl"))
            .status()
            .unwrap();
        assert!(status.success(), "dxc failed");
        let dxil = std::fs::read(path("shader.dxil")).unwrap();
        // The container holds its root signature: it serves as one.
        std::fs::write(path("root-signature.bin"), &dxil).unwrap();
        let shader = HostShader {
            stage: victory_shader::HwStage::Cs,
            dxil,
            user_data: Vec::new(),
            samplers: Vec::new(),
            static_samplers: Vec::new(),
            root_cbvs: Vec::new(),
            root_srvs: Vec::new(),
            root_uavs: Vec::new(),
            root_indexed_views: Vec::new(),
        };
        std::fs::write(
            path("host-shader.bin"),
            crate::shader_cache::encode(&shader),
        )
        .unwrap();
        let texels: Vec<f32> = (0..8).map(|i| i as f32 * 1.5).collect();
        std::fs::write(
            path("r0-before.bin"),
            texels
                .iter()
                .flat_map(|t| t.to_le_bytes())
                .collect::<Vec<u8>>(),
        )
        .unwrap();
        std::fs::write(path("r1-before.bin"), [0xeeu8; 32]).unwrap();
        let wrote: Vec<u8> = texels
            .iter()
            .flat_map(|t| (t.to_bits() + 100).to_le_bytes())
            .collect();
        let uav = D3D12_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT_R32_TYPELESS,
            ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
            Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                Buffer: D3D12_BUFFER_UAV {
                    NumElements: 8,
                    Flags: D3D12_BUFFER_UAV_FLAG_RAW,
                    ..Default::default()
                },
            },
        };
        // SAFETY: a plain description's bytes.
        let uav = hex(unsafe {
            std::slice::from_raw_parts(std::ptr::from_ref(&uav).cast::<u8>(), size_of_val(&uav))
        });
        let manifest = format!(
            r#"{{"format": 1, "shader": "test", "frame": 1, "translation": {{}},
            "heap": {{"slots": 16, "increment": 32}}, "groups": [[1, 1, 1, 0]], "group_base": null,
            "resources": [
              {{"id": 0, "desc": {{"dimension": 3, "width": 8, "height": 1, "depth_or_array": 1, "mips": 1, "format": 41, "samples": 1, "flags": 0}},
                "written": false, "before": "r0-before.bin"}},
              {{"id": 1, "desc": {{"dimension": 1, "width": 32, "height": 1, "depth_or_array": 1, "mips": 1, "format": 0, "samples": 1, "flags": 4}},
                "written": true, "before": "r1-before.bin", "after": "r1-after.bin"}}],
            "slots": [{{"slot": 5, "kind": "srv", "desc": null, "resource": 0}}, {{"slot": 9, "kind": "uav", "desc": "{uav}", "resource": 1}}],
            "roots": [{{"param": 0, "constants": [5, 9, 100, 0]}}], "notes": []}}"#
        );
        std::fs::write(path("manifest.json"), manifest).unwrap();
        let options = Options {
            variants: 1,
            runs: 2,
            dxil: None,
            retranslate: false,
            show: 2,
            diffs: None,
            split: 0,
            sensitivity: false,
            background: false,
        };
        std::fs::write(path("r1-after.bin"), &wrote).unwrap();
        match replay_dispatch(dir.path(), &options) {
            Ok(summary) => assert_eq!(
                summary,
                Summary {
                    runs: 2,
                    unlike_capture: 0,
                    unlike_first: 0
                }
            ),
            Err(e) if format!("{e:#}").contains("no D3D12 device") => {
                eprintln!("no D3D12 device; skipped");
                return;
            }
            Err(e) => panic!("{e:#}"),
        }
        let mut other = wrote.clone();
        other[4] ^= 1;
        std::fs::write(path("r1-after.bin"), &other).unwrap();
        let summary = replay_dispatch(dir.path(), &options).unwrap();
        assert_eq!(
            summary,
            Summary {
                runs: 2,
                unlike_capture: 2,
                unlike_first: 0
            }
        );
    }

    /// Packed rows go into a footprint's padded rows and back.
    #[test]
    fn rows_pack_and_unpack() {
        let mut fp = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
        fp.Footprint.Width = 2;
        fp.Footprint.Height = 2;
        fp.Footprint.Depth = 1;
        fp.Footprint.RowPitch = 256;
        let l = Layout {
            footprints: vec![fp],
            rows: vec![2],
            row_bytes: vec![8],
            total: 512,
        };
        let packed: Vec<u8> = (0..16).collect();
        let mut data = vec![0u8; 512];
        l.unpack(&packed, &mut data);
        assert_eq!(&data[256..264], &packed[8..16]);
        assert_eq!(l.pack(&data), packed);
        assert_eq!(l.place(12), (0, 1, 1, 0));
    }
}
