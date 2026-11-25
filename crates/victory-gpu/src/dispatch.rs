//! One compute dispatch captured whole, to run it again offline as often as
//! needed (`victory replay-dispatch`, [`crate::replay`]). Only in builds
//! with diagnostics.
//!
//! `VICTORY_DISPATCH_CAPTURE=<shader hash prefix>:<frame>:<folder>`: the
//! first dispatch, direct or indirect, of a compute pipeline whose shader's
//! hash starts with the prefix, in that frame or the first one after it that
//! runs one, is written to the folder (`manifest.json` says what is where).
//! The frame `w<n>` is the `n`th after the world's first (with
//! `VICTORY_CAPTURE_WORLD`, `crate::capture`): how fast a title's loading
//! screens run moves its frame numbers from run to run. Written are
//! - the translated code, the console's container (to translate it again),
//!   the binding layout and our root signature;
//! - the root arguments as the host list was given them, with the per-dispatch
//!   words the host computes (the first group, the thread count);
//! - the group counts, an indirect execution's as the card read them;
//! - what every resource the dispatch can reach held just before it ran
//!   (copied on the card, in the dispatch's own list), and what the ones it
//!   can write held just after.
//!
//! A dispatch reaches its resources through the root arguments and through
//! the view heap, by indices made from its user data: the slots captured are
//! the root tables' and, per table the user data names, the run of slots from
//! the table to the last entry the shader's used resources and root-indexed
//! views name (as `State::tables` makes a fresh table). Host views are noted
//! as they are made ([`note_view`], from the moment the variable is read), so
//! the replay makes each slot again from its description.
//!
//! shortcut: sampler heap tables, title memory by address (`SLOT_MEMORY`),
//! buffers picked from the whole heap (`SLOT_STRIDES`) and multisampled
//! textures are not captured (the manifest's notes say so); slots past the
//! runs above read as null in the replay.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use victory_shader::RootType;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::Interface;

use crate::host::HostList;
use crate::pipeline::host::HostPipeline;
use crate::replay::Json;

/// A host view as it was made in a heap slot of ours (`descriptor::host`'s
/// `create_srv` and the like): its description; `None` for the resource's
/// default view.
#[derive(Clone, Copy)]
pub(crate) enum Made {
    Srv(Option<D3D12_SHADER_RESOURCE_VIEW_DESC>),
    Uav(Option<D3D12_UNORDERED_ACCESS_VIEW_DESC>),
    Cbv(D3D12_CONSTANT_BUFFER_VIEW_DESC),
    Sampler(D3D12_SAMPLER_DESC),
}

// SAFETY: plain descriptions (the CBV's address is a number).
unsafe impl Send for Made {}

/// A root argument as `State::bind` set it on the host list.
pub(crate) enum RootArg {
    Constants(Vec<u32>),
    /// A root descriptor: "cbv", "srv" or "uav", its host address, and the
    /// title address it shows (none: a buffer of ours).
    View(&'static str, u64, Option<u64>),
    /// A descriptor table: its first slot's shader-visible handle.
    Table(u64),
}

/// The group counts of the dispatch captured.
pub(crate) enum Groups<'a> {
    Direct([u32; 3]),
    /// An indirect execution: its argument buffer and where the records
    /// start, how many it may run and how far apart; `wide`: the records of
    /// `crate::indirect::bound_wide` (two commands of 16 bytes each, a first
    /// group and a dispatch); the count buffer and its offset.
    Indirect {
        args: &'a ID3D12Resource,
        at: u64,
        records: u32,
        stride: u32,
        wide: bool,
        count: Option<(&'a ID3D12Resource, u64)>,
    },
}

/// What `VICTORY_DISPATCH_CAPTURE` asks for: the frame, counted from the
/// world's first if `world`.
struct Plan {
    prefix: String,
    frame: u64,
    world: bool,
    folder: PathBuf,
}

fn plan() -> Option<&'static Plan> {
    static PLAN: std::sync::OnceLock<Option<Plan>> = std::sync::OnceLock::new();
    PLAN.get_or_init(|| {
        let value = std::env::var("VICTORY_DISPATCH_CAPTURE").ok()?;
        // The folder may hold a drive letter's colon: two splits only.
        let mut parts = value.splitn(3, ':');
        let (prefix, frame, folder) = (parts.next()?, parts.next()?, parts.next()?);
        let (world, frame) = match frame.trim().strip_prefix('w') {
            Some(after) => (true, after),
            None => (false, frame.trim()),
        };
        let Ok(frame) = frame.parse() else {
            tracing::warn!(value, "VICTORY_DISPATCH_CAPTURE: <shader hash prefix>:<frame or w<frames after the world's first>>:<folder>");
            return None;
        };
        if world && std::env::var_os("VICTORY_CAPTURE_WORLD").is_none() {
            tracing::warn!("VICTORY_DISPATCH_CAPTURE counts from the world's first frame, which only VICTORY_CAPTURE_WORLD finds");
        }
        tracing::info!(prefix, frame, world, folder, "a dispatch will be captured");
        Some(Plan {
            prefix: prefix.trim().to_ascii_lowercase(),
            frame,
            world,
            folder: PathBuf::from(folder),
        })
    })
    .as_ref()
}

/// The capture was taken (or given up): nothing more is noted.
static TAKEN: AtomicBool = AtomicBool::new(false);

/// Whether views and root arguments are noted: a capture is asked for and
/// not taken yet.
pub(crate) fn armed() -> bool {
    plan().is_some() && !TAKEN.load(Ordering::Relaxed)
}

/// Host views by CPU handle, as last made ([`note_view`]).
static VIEWS: Mutex<Option<HashMap<usize, Made>>> = Mutex::new(None);

/// Notes the view about to be made at `at`, while a capture is armed.
pub(crate) fn note_view(at: D3D12_CPU_DESCRIPTOR_HANDLE, made: impl FnOnce() -> Made) {
    if !armed() {
        return;
    }
    VIEWS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(HashMap::new)
        .insert(at.ptr, made());
}

/// Whether the dispatch about to be bound with `p` is the one to capture.
pub(crate) fn wants(p: &HostPipeline) -> bool {
    let Some(plan) = plan() else { return false };
    let from = if plan.world {
        match crate::capture::world() {
            Some(first) => first + plan.frame,
            None => return false,
        }
    } else {
        plan.frame
    };
    armed()
        && p.compute
        && crate::queue::presented() + 1 >= from
        && p.stages
            .first()
            .is_some_and(|s| s.shader.hash.starts_with(&plan.prefix))
}

/// The capture recorded and not yet written: written at a present once its
/// list has run ([`presented`]).
static PENDING: Mutex<Option<Capture>> = Mutex::new(None);

/// Bytes the list copies last, to tell that it ran.
const MARKER: [u8; 16] = *b"victory capture\0";

/// One resource the dispatch can reach.
struct Res {
    resource: ID3D12Resource,
    desc: D3D12_RESOURCE_DESC,
    /// A buffer's bytes from `base` on, `len` of them (the views' span).
    base: u64,
    len: u64,
    /// Element sizes its buffer views count in (`base` is a multiple of
    /// each; 0: a view of elements of unknown size, and `base` stays 0).
    elements: Vec<u64>,
    /// A texture's subresources as the read-back holds them.
    layout: Option<Layout>,
    /// The dispatch can write it.
    written: bool,
    /// It has a view the dispatch reads or writes as a texture or buffer
    /// (else only constants).
    read_as: &'static str,
    notes: Vec<String>,
    before: Option<ID3D12Resource>,
    after: Option<ID3D12Resource>,
}

/// A texture's subresources in a read-back buffer: their footprints, rows
/// and bytes per row, and the buffer's size.
struct Layout {
    footprints: Vec<D3D12_PLACED_SUBRESOURCE_FOOTPRINT>,
    rows: Vec<u32>,
    row_bytes: Vec<u64>,
    total: u64,
}

impl Layout {
    /// The subresources' rows without the footprints' padding.
    fn pack(&self, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        for (i, f) in self.footprints.iter().enumerate() {
            let pitch = f.Footprint.RowPitch as usize;
            for z in 0..f.Footprint.Depth as usize {
                for row in 0..self.rows[i] as usize {
                    let at = f.Offset as usize + (z * self.rows[i] as usize + row) * pitch;
                    out.extend_from_slice(&data[at..at + self.row_bytes[i] as usize]);
                }
            }
        }
        out
    }
}

/// A view slot the dispatch can reach.
struct Slot {
    index: u32,
    made: Option<Made>,
    /// The resource it views (index into the capture's), and a buffer
    /// view's or constant view's byte offset from the captured bytes' start.
    resource: Option<usize>,
    offset: u64,
}

/// The dispatch being captured.
pub(crate) struct Capture {
    frame: u64,
    folder: PathBuf,
    /// Files written as they are.
    files: Vec<(&'static str, Vec<u8>)>,
    manifest: Vec<(String, Json)>,
    roots: Vec<(u32, RootArg, Option<(usize, u64)>)>,
    slots: Vec<Slot>,
    resources: Vec<Res>,
    /// An indirect execution's records as the card read them: the read-back
    /// and its bytes, whether wide, the stride, and the count's place.
    args: Option<(ID3D12Resource, u64, bool, u32, bool)>,
    marker: (ID3D12Resource, ID3D12Resource),
    /// The frame at whose present the marker was first seen.
    landed: Option<u64>,
    /// The view heap's first shader-visible handle and its step.
    heap: (u64, u64),
    notes: Vec<String>,
}

// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for Capture {}

/// Bytes of a typed buffer element of `format` (0: not known).
fn format_bytes(format: DXGI_FORMAT) -> u64 {
    match format.0 {
        1..=4 => 16,
        5..=8 => 12,
        9..=22 => 8,
        23..=47 | 67..=68 | 87..=93 => 4,
        48..=59 | 85 | 86 | 115 => 2,
        60..=65 => 1,
        _ => 0,
    }
}

/// The bytes of a buffer view's element (0: not known).
pub(crate) fn element(stride: u32, raw: bool, format: DXGI_FORMAT) -> u64 {
    if stride > 0 {
        u64::from(stride)
    } else if raw {
        4
    } else {
        format_bytes(format)
    }
}

/// A buffer view's element size and its first byte and bytes (an element
/// of unknown size: 0, the whole buffer).
fn buffer_span(
    first: u64,
    count: u32,
    stride: u32,
    raw: bool,
    format: DXGI_FORMAT,
) -> (u64, u64, u64) {
    let element = element(stride, raw, format);
    if element == 0 {
        return (0, 0, u64::MAX);
    }
    (element, first * element, u64::from(count) * element)
}

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 { a } else { gcd(b, a % b) }
}

/// Starts capturing the dispatch `p` is bound for in `list`: the host list
/// has every root argument (`roots`, as `State::bind` set them) and the
/// heaps; the copies of what it can reach are recorded here, before the
/// dispatch. `heap`: the bound view heap; `constant`: a title root
/// constant (slot, dword), for root-indexed views. `None` if another
/// thread took the capture first or the host cannot record it.
pub(crate) fn begin(
    list: &HostList,
    p: &HostPipeline,
    roots: Vec<(u32, RootArg)>,
    heap: Option<&crate::descriptor::host::HostHeap>,
    constant: &dyn Fn(u8, u32) -> u32,
    groups: Groups,
) -> Option<Capture> {
    if TAKEN.swap(true, Ordering::Relaxed) {
        return None;
    }
    let plan = plan()?;
    let host = crate::host::host()?;
    let device = &host.device;
    let [stage] = &p.stages[..] else { return None };
    let t = stage.translated();
    let frame = crate::queue::presented() + 1;
    let mut notes = Vec::new();
    tracing::info!(pipeline = %p.name, frame, "capturing a dispatch");

    // Where the slots are: the view heap's shader-visible start and step.
    let (gpu_start, step) = heap.map_or((0, 0), |h| {
        let start = h.gpu_handle(0).ptr;
        (start, h.gpu_handle(1).ptr - start)
    });
    let values: Vec<u32> = match stage.user_data {
        Some((param, _)) => roots
            .iter()
            .find_map(|(at, r)| match r {
                RootArg::Constants(v) if *at == param => Some(v.clone()),
                _ => None,
            })
            .unwrap_or_default(),
        None => Vec::new(),
    };
    // The slots it can reach.
    let mut indices = std::collections::BTreeSet::new();
    for (_, root) in &roots {
        if let RootArg::Table(handle) = root
            && step > 0
            && *handle >= gpu_start
        {
            indices.insert(((handle - gpu_start) / step) as u32);
        }
    }
    for u in t
        .host
        .user_data
        .iter()
        .filter(|u| u.root == RootType::Table)
    {
        let Some(&value) = values.get(u.index as usize) else {
            continue;
        };
        let entries: Vec<_> = t
            .used
            .iter()
            .filter(|e| e.root == RootType::Table && e.slot == u.slot)
            .collect();
        if entries
            .iter()
            .any(|e| e.kind == victory_shader::ResourceType::Sampler)
        {
            notes.push(format!(
                "user data {} is a sampler table: not captured",
                u.index
            ));
            continue;
        }
        let last = entries
            .iter()
            .map(|e| e.table_offset_dwords * 4)
            .chain(
                t.host
                    .root_indexed_views
                    .iter()
                    .filter(|v| v.table_slot == u.slot)
                    .map(|v| {
                        v.byte_offset
                            .wrapping_add(constant(v.constant_slot, v.constant_dword) << v.shift)
                    }),
            )
            .max()
            .unwrap_or(0);
        for k in 0..=last / 32 {
            indices.insert(value / 32 + k);
        }
    }
    if t.host
        .root_srvs
        .contains(&victory_shader::emit::SLOT_MEMORY)
    {
        notes.push("the shader reads title memory by address: not captured".into());
    }
    if t.host
        .root_srvs
        .contains(&victory_shader::emit::SLOT_STRIDES)
    {
        notes.push(
            "the shader picks buffers from the whole heap: only the slots above are captured"
                .into(),
        );
    }

    let mut resources: Vec<Res> = Vec::new();
    // The resource's entry, made on first sight; `span`: buffer bytes it
    // needs (element size, first byte, bytes).
    let enter = |resources: &mut Vec<Res>,
                 r: &ID3D12Resource,
                 span: Option<(u64, u64, u64)>,
                 written: bool,
                 read_as: &'static str|
     -> usize {
        let at = match resources
            .iter()
            .position(|x| x.resource.as_raw() == r.as_raw())
        {
            Some(at) => at,
            None => {
                // SAFETY: a live resource.
                let desc = unsafe { r.GetDesc() };
                resources.push(Res {
                    resource: r.clone(),
                    desc,
                    base: u64::MAX,
                    len: 0,
                    elements: Vec::new(),
                    layout: None,
                    written: false,
                    read_as: "constants",
                    notes: Vec::new(),
                    before: None,
                    after: None,
                });
                resources.len() - 1
            }
        };
        let res = &mut resources[at];
        res.written |= written;
        if read_as != "constants" {
            res.read_as = read_as;
        }
        if res.desc.Dimension == D3D12_RESOURCE_DIMENSION_BUFFER {
            let (element, first, bytes) = span.unwrap_or((1, 0, u64::MAX));
            let end = first.saturating_add(bytes).min(res.desc.Width);
            let end_now = if res.base == u64::MAX {
                0
            } else {
                res.base + res.len
            };
            res.base = res.base.min(first);
            res.len = end.max(end_now) - res.base;
            if !res.elements.contains(&element) {
                res.elements.push(element);
            }
        }
        at
    };

    // The slots: their views and resources.
    let mut slots = Vec::new();
    {
        let views = VIEWS.lock().unwrap_or_else(|e| e.into_inner());
        for &index in &indices {
            let Some((at, kept)) = heap.and_then(|h| h.captured_slot(index)) else {
                continue;
            };
            let made = views.as_ref().and_then(|v| v.get(&at.ptr)).copied();
            let mut slot = Slot {
                index,
                made,
                resource: None,
                offset: 0,
            };
            match (made, &kept) {
                (Some(Made::Srv(desc)), Some(r)) => {
                    // SAFETY: a buffer view's description holds the buffer fields.
                    let span = desc
                        .filter(|d| d.ViewDimension == D3D12_SRV_DIMENSION_BUFFER)
                        .map(|d| unsafe {
                            let b = d.Anonymous.Buffer;
                            buffer_span(
                                b.FirstElement,
                                b.NumElements,
                                b.StructureByteStride,
                                b.Flags.0 & 1 != 0,
                                d.Format,
                            )
                        });
                    slot.resource = Some(enter(&mut resources, r, span, false, "read"));
                }
                (Some(Made::Uav(desc)), Some(r)) => {
                    // SAFETY: as above.
                    let span = desc
                        .filter(|d| d.ViewDimension == D3D12_UAV_DIMENSION_BUFFER)
                        .map(|d| unsafe {
                            let b = d.Anonymous.Buffer;
                            buffer_span(
                                b.FirstElement,
                                b.NumElements,
                                b.StructureByteStride,
                                b.Flags.0 & 1 != 0,
                                d.Format,
                            )
                        });
                    slot.resource = Some(enter(&mut resources, r, span, true, "written"));
                }
                (Some(Made::Cbv(desc)), _) if desc.BufferLocation != 0 => {
                    // Constants in a host copy (the slot holds it), or in the
                    // list's scratch memory (a table made afresh).
                    let found = kept
                        .as_ref()
                        .and_then(|r| {
                            // SAFETY: a live buffer.
                            let base = unsafe { r.GetGPUVirtualAddress() };
                            // SAFETY: as above.
                            let width = unsafe { r.GetDesc() }.Width;
                            (desc.BufferLocation >= base && desc.BufferLocation < base + width)
                                .then(|| (r.clone(), desc.BufferLocation - base))
                        })
                        .or_else(|| list.scratch_at(desc.BufferLocation));
                    match found {
                        Some((r, offset)) => {
                            let at = enter(
                                &mut resources,
                                &r,
                                Some((256, offset, u64::from(desc.SizeInBytes))),
                                false,
                                "constants",
                            );
                            slot.resource = Some(at);
                            slot.offset = offset;
                        }
                        None => notes.push(format!(
                            "slot {index}: constants at {:#x} not found",
                            desc.BufferLocation
                        )),
                    }
                }
                (None, _) => {
                    notes.push(format!("slot {index}: no view noted (null in the replay)"))
                }
                _ => {}
            }
            slots.push(slot);
        }
    }

    // Root descriptors: their buffers.
    let mut root_out = Vec::new();
    for (param, root) in roots {
        let mut found = None;
        if let RootArg::View(kind, gpu, title) = &root {
            let candidate = list.scratch_at(*gpu).or_else(|| {
                let shadow = crate::shadow::find((*title)?, 1)?;
                // SAFETY: a live buffer.
                let base = unsafe { shadow.buffer.GetGPUVirtualAddress() };
                // SAFETY: as above.
                let width = unsafe { shadow.buffer.GetDesc() }.Width;
                (*gpu >= base && *gpu < base + width).then(|| (shadow.buffer.clone(), gpu - base))
            });
            match candidate {
                Some((r, offset)) => {
                    let bytes = if *kind == "cbv" { 1 << 16 } else { u64::MAX };
                    let element = if *kind == "cbv" { 256 } else { 4 };
                    let read_as = if *kind == "uav" { "written" } else { "read" };
                    let at = enter(
                        &mut resources,
                        &r,
                        Some((element, offset, bytes)),
                        *kind == "uav",
                        read_as,
                    );
                    found = Some((at, offset));
                }
                None => notes.push(format!(
                    "root parameter {param}: {kind} at {gpu:#x} not found (a buffer of ours)"
                )),
            }
        }
        root_out.push((param, root, found));
    }

    // The bytes to copy: a buffer from a start that keeps every view's
    // first element whole; a texture whole.
    for res in &mut resources {
        if res.desc.Dimension == D3D12_RESOURCE_DIMENSION_BUFFER {
            let mut align = 256u64;
            for &e in &res.elements {
                align = if e == 0 {
                    u64::MAX
                } else {
                    align / gcd(align, e) * e
                };
            }
            let end = res.base + res.len;
            res.base = if align == u64::MAX {
                0
            } else {
                res.base - res.base % align
            };
            res.len = (end - res.base).max(4);
            continue;
        }
        if res.desc.SampleDesc.Count > 1 {
            res.notes.push("multisampled: not captured".into());
            continue;
        }
        let mut info = D3D12_FEATURE_DATA_FORMAT_INFO {
            Format: res.desc.Format,
            PlaneCount: 1,
        };
        // SAFETY: the structure the feature asks for.
        let planes = unsafe {
            device.CheckFeatureSupport(
                D3D12_FEATURE_FORMAT_INFO,
                (&raw mut info).cast(),
                size_of_val(&info) as u32,
            )
        }
        .map_or(1, |()| u32::from(info.PlaneCount.max(1)));
        let arrays = if res.desc.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D {
            1
        } else {
            u32::from(res.desc.DepthOrArraySize)
        };
        let n = u32::from(res.desc.MipLevels.max(1)) * arrays * planes;
        let mut layout = Layout {
            footprints: vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); n as usize],
            rows: vec![0; n as usize],
            row_bytes: vec![0; n as usize],
            total: 0,
        };
        // SAFETY: outputs for `n` subresources.
        unsafe {
            device.GetCopyableFootprints(
                &res.desc,
                0,
                n,
                0,
                Some(layout.footprints.as_mut_ptr()),
                Some(layout.rows.as_mut_ptr()),
                Some(layout.row_bytes.as_mut_ptr()),
                Some(&mut layout.total),
            );
        }
        res.layout = Some(layout);
    }

    // The copies before the dispatch: after everything the list ran so far.
    // SAFETY: an open list.
    unsafe { list.list.ResourceBarrier(&[uav_barrier()]) };
    for res in &mut resources {
        res.before = copy_out(host, list, res);
    }
    let args = match groups {
        Groups::Direct(_) => None,
        Groups::Indirect {
            args,
            at,
            records,
            stride,
            wide,
            count,
        } => {
            let (from, bytes) = if wide {
                (0, u64::from(records) * 32)
            } else {
                (at, u64::from(records) * u64::from(stride))
            };
            let back = host.read_back_buffer(bytes + 16)?;
            // SAFETY: an open list; live buffers holding the ranges.
            unsafe {
                list.list.CopyBufferRegion(&back, 0, args, from, bytes);
                if let Some((buffer, offset)) = count {
                    list.list.CopyBufferRegion(&back, bytes, buffer, offset, 4);
                }
            }
            list.keep(&back);
            Some((back, bytes, wide, stride, count.is_some()))
        }
    };

    let mut manifest: Vec<(String, Json)> = Vec::new();
    let mut put = |k: &str, v: Json| manifest.push((k.to_owned(), v));
    put("format", Json::Int(1));
    put("pipeline", Json::Str(p.name.clone()));
    put("shader", Json::Str(stage.shader.hash.clone()));
    put("frame", Json::Int(frame as i64));
    put(
        "translation",
        Json::Map(vec![
            (
                "view_heap_limit".into(),
                Json::Int(i64::from(victory_shader::emit::view_heap_limit())),
            ),
            (
                "host_max_wave".into(),
                Json::Int(i64::from(victory_shader::emit::host_max_wave())),
            ),
            (
                "memory_pages".into(),
                Json::Bool(victory_shader::emit::memory_pages()),
            ),
            (
                "group_sync".into(),
                Json::Int(i64::from(victory_shader::emit::group_sync())),
            ),
            (
                "loop_limit".into(),
                Json::Int(i64::from(victory_shader::emit::loop_limit())),
            ),
        ]),
    );
    put(
        "heap",
        Json::Map(vec![
            (
                "slots".into(),
                Json::Int(heap.map_or(0, |h| i64::from(h.size()))),
            ),
            ("increment".into(), Json::Int(step as i64)),
        ]),
    );
    if let Groups::Direct([x, y, z]) = groups {
        put("groups", Json::List(vec![Json::ints(&[x, y, z, 0])]));
    }
    let group_base = stage.user_data.and_then(|(param, _)| {
        let base = RootType::Other(victory_shader::emit::USER_GROUP_BASE);
        t.host
            .user_data
            .iter()
            .find(|u| u.root == base)
            .map(|u| Json::ints(&[param, u.index]))
    });
    put("group_base", group_base.unwrap_or(Json::Null));
    put(
        "user_data",
        crate::replay::user_data_json(&t.host.user_data, &values),
    );
    put(
        "used",
        Json::List(t.used.iter().map(|u| Json::Str(format!("{u:?}"))).collect()),
    );

    let marker = host.upload_buffer(256).and_then(|(upload, data)| {
        // SAFETY: the upload buffer's mapping, 256 bytes.
        unsafe { std::ptr::copy_nonoverlapping(MARKER.as_ptr(), data, MARKER.len()) };
        Some((upload, host.read_back_buffer(256)?))
    })?;
    list.keep(&marker.0);
    list.keep(&marker.1);
    let files = vec![
        ("shader.dxil", t.host.dxil.clone()),
        ("shader.cdxil", stage.shader.container().to_vec()),
        ("root-signature.bin", p.root_blob.clone()),
        ("host-shader.bin", crate::shader_cache::encode(&t.host)),
    ];
    Some(Capture {
        frame,
        folder: plan.folder.clone(),
        files,
        manifest,
        roots: root_out,
        slots,
        resources,
        args,
        marker,
        landed: None,
        heap: (gpu_start, step),
        notes,
    })
}

impl Capture {
    /// Records the copies after the dispatch (what it can write, then the
    /// marker), and leaves the capture to be written once the list ran.
    pub(crate) fn end(mut self, list: &HostList) {
        let Some(host) = crate::host::host() else {
            return;
        };
        // SAFETY: an open list.
        unsafe { list.list.ResourceBarrier(&[uav_barrier()]) };
        for res in self.resources.iter_mut().filter(|r| r.written) {
            res.after = copy_out(host, list, res);
        }
        // SAFETY: an open list; buffers of ours.
        unsafe {
            list.list
                .CopyBufferRegion(&self.marker.1, 0, &self.marker.0, 0, MARKER.len() as u64)
        };
        tracing::info!(
            resources = self.resources.len(),
            slots = self.slots.len(),
            "dispatch capture recorded; written once its list has run"
        );
        *PENDING.lock().unwrap_or_else(|e| e.into_inner()) = Some(self);
    }

    /// Whether the list ran (the marker landed).
    fn landed(&self) -> bool {
        read(&self.marker.1, MARKER.len()).is_some_and(|b| b == MARKER)
    }

    fn write(self) -> std::io::Result<PathBuf> {
        let folder = self.folder;
        std::fs::create_dir_all(&folder)?;
        for (name, bytes) in &self.files {
            std::fs::write(folder.join(name), bytes)?;
        }
        let mut manifest = self.manifest;
        let mut notes = self.notes;
        // The resources.
        let mut list = Vec::new();
        for (i, res) in self.resources.iter().enumerate() {
            let mut fields = vec![("id".to_owned(), Json::Int(i as i64))];
            let d = &res.desc;
            let buffer = d.Dimension == D3D12_RESOURCE_DIMENSION_BUFFER;
            fields.push((
                "desc".into(),
                crate::replay::desc_json(d, buffer.then_some(res.len)),
            ));
            fields.push(("use".into(), Json::Str(res.read_as.into())));
            fields.push(("written".into(), Json::Bool(res.written)));
            if buffer {
                fields.push(("from".into(), Json::Str(format!("{:#x}", res.base))));
            }
            for (what, back) in [("before", &res.before), ("after", &res.after)] {
                let Some(back) = back else { continue };
                let bytes = match &res.layout {
                    Some(layout) => read(back, layout.total as usize).map(|b| layout.pack(&b)),
                    None => read(back, res.len as usize),
                };
                let Some(bytes) = bytes else {
                    notes.push(format!("resource {i}: {what} not read back"));
                    continue;
                };
                let name = format!("r{i}-{what}.bin");
                std::fs::write(folder.join(&name), &bytes)?;
                fields.push((what.into(), Json::Str(name)));
            }
            if !res.notes.is_empty() {
                fields.push((
                    "notes".into(),
                    Json::List(res.notes.iter().cloned().map(Json::Str).collect()),
                ));
            }
            list.push(Json::Map(fields));
        }
        manifest.push(("resources".into(), Json::List(list)));
        // The slots, their views rebased on the bytes captured.
        let views: Vec<Json> = self
            .slots
            .iter()
            .map(|s| {
                let base = s.resource.map_or(0, |r| self.resources[r].base);
                crate::replay::slot_json(
                    s.index,
                    s.made.as_ref(),
                    s.resource,
                    s.offset.wrapping_sub(base),
                    base,
                )
            })
            .collect();
        manifest.push(("slots".into(), Json::List(views)));
        let roots: Vec<Json> = self
            .roots
            .iter()
            .map(|(param, root, found)| {
                let mut f = vec![("param".to_owned(), Json::Int(i64::from(*param)))];
                match root {
                    RootArg::Constants(v) => f.push(("constants".into(), Json::ints(v))),
                    RootArg::Table(handle) => {
                        f.push(("table".into(), Json::Str(format!("{handle:#x}"))));
                        let (start, step) = self.heap;
                        let slot = handle
                            .checked_sub(start)
                            .map_or(-1, |at| (at / step.max(1)) as i64);
                        f.push(("slot".into(), Json::Int(slot)));
                    }
                    RootArg::View(kind, gpu, _) => {
                        f.push(("view".into(), Json::Str((*kind).into())));
                        f.push(("address".into(), Json::Str(format!("{gpu:#x}"))));
                        if let Some((r, offset)) = found {
                            f.push(("resource".into(), Json::Int(*r as i64)));
                            f.push((
                                "offset".into(),
                                Json::Int((offset - self.resources[*r].base) as i64),
                            ));
                        }
                    }
                }
                Json::Map(f)
            })
            .collect();
        manifest.push(("roots".into(), Json::List(roots)));
        // An indirect execution's records, as the dispatches they ran.
        if let Some((back, bytes, wide, stride, counted)) = &self.args {
            let raw = read(back, *bytes as usize + 4).unwrap_or_default();
            let word = |at: usize| {
                raw.get(at..at + 4)
                    .map_or(0, |w| u32::from_le_bytes(w.try_into().unwrap_or_default()))
            };
            let mut groups = Vec::new();
            if *wide {
                for c in (0..*bytes as usize).step_by(16) {
                    let [base, x, y, z] = [0, 4, 8, 12].map(|k| word(c + k));
                    if x * y * z > 0 {
                        groups.push(Json::ints(&[x, y, z, base]));
                    }
                }
            } else {
                let records = *bytes / u64::from((*stride).max(1));
                let n = if *counted {
                    u64::from(word(*bytes as usize)).min(records)
                } else {
                    records
                };
                for r in 0..n as usize {
                    let at = r * *stride as usize;
                    let [x, y, z] = [0, 4, 8].map(|k| word(at + k));
                    if x * y * z > 0 {
                        groups.push(Json::ints(&[x, y, z, 0]));
                    }
                }
            }
            manifest.push((
                "indirect".into(),
                Json::Map(vec![
                    ("wide".into(), Json::Bool(*wide)),
                    ("counted".into(), Json::Bool(*counted)),
                ]),
            ));
            manifest.push(("groups".into(), Json::List(groups)));
        }
        manifest.push((
            "notes".into(),
            Json::List(notes.into_iter().map(Json::Str).collect()),
        ));
        let text = Json::Map(manifest).to_text();
        std::fs::write(folder.join("manifest.json"), text)?;
        Ok(folder)
    }
}

/// Records a copy of what `res` holds now into a read-back buffer of ours,
/// which the list keeps: a buffer's captured bytes, a texture's
/// subresources (moved to the copy state and back if the list tracks it).
fn copy_out(host: &crate::host::Host, list: &HostList, res: &mut Res) -> Option<ID3D12Resource> {
    const COPY_SOURCE: u32 = 0x800;
    let r = &res.resource;
    let back = match &res.layout {
        None if res.desc.Dimension == D3D12_RESOURCE_DIMENSION_BUFFER => {
            let back = host.read_back_buffer(res.len)?;
            // SAFETY: an open list; the range lies in the buffer.
            unsafe { list.list.CopyBufferRegion(&back, 0, r, res.base, res.len) };
            back
        }
        None => return None,
        Some(layout) => {
            let back = host.read_back_buffer(layout.total)?;
            let state = list.move_tracked(r, COPY_SOURCE);
            if state.is_none() && !res.notes.iter().any(|n| n.starts_with("copied")) {
                res.notes
                    .push("copied in a state the list does not track".into());
            }
            for (i, footprint) in layout.footprints.iter().enumerate() {
                let to = D3D12_TEXTURE_COPY_LOCATION {
                    // SAFETY: a borrowed pointer the location never releases.
                    pResource: unsafe { std::mem::transmute_copy(&back) },
                    Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                        PlacedFootprint: *footprint,
                    },
                };
                let from = D3D12_TEXTURE_COPY_LOCATION {
                    // SAFETY: as above.
                    pResource: unsafe { std::mem::transmute_copy(r) },
                    Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                        SubresourceIndex: i as u32,
                    },
                };
                // SAFETY: an open list; whole subresources of live resources.
                unsafe { list.list.CopyTextureRegion(&to, 0, 0, 0, &from, None) };
            }
            if let Some(state) = state {
                list.move_tracked(r, state);
            }
            back
        }
    };
    list.keep(&back);
    list.keep(r);
    Some(back)
}

/// The first `len` bytes of a read-back buffer of ours.
fn read(buffer: &ID3D12Resource, len: usize) -> Option<Vec<u8>> {
    let mut data = std::ptr::null_mut();
    // SAFETY: a read-back buffer at least `len` long; mapped, read, unmapped.
    unsafe {
        buffer.Map(0, None, Some(&mut data)).ok()?;
        let bytes = std::slice::from_raw_parts(data.cast::<u8>(), len).to_vec();
        buffer.Unmap(0, None);
        Some(bytes)
    }
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

/// At the present of `frame`: writes the capture recorded, two presents
/// after its list's last copy landed (the copies before it may still run
/// when it does); given up 600 frames on.
pub(crate) fn presented(frame: u64) {
    let mut pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    let Some(capture) = pending.as_mut() else {
        return;
    };
    if capture.landed.is_none() && capture.landed() {
        capture.landed = Some(frame);
    }
    if capture.landed.is_none_or(|at| frame < at + 2) {
        if frame > capture.frame + 600 {
            tracing::warn!("the captured dispatch's list never ran: capture dropped");
            *pending = None;
        }
        return;
    }
    let Some(capture) = pending.take() else {
        return;
    };
    drop(pending);
    // Nothing more is noted.
    *VIEWS.lock().unwrap_or_else(|e| e.into_inner()) = None;
    match capture.write() {
        Ok(folder) => tracing::info!(folder = %folder.display(), "dispatch captured"),
        Err(e) => tracing::warn!(error = %e, "dispatch capture not written"),
    }
}
