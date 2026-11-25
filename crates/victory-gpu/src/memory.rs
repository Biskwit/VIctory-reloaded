//! Title memory by address, for translated shaders that read and write
//! through raw 40-bit addresses (`victory_shader::emit::SLOT_MEMORY`: the
//! console's ray-tracing structure builder and the programs that walk its
//! structures; the structures themselves lie in plain title memory, in no
//! buffer object).
//!
//! The copies are the ones every other access uses ([`crate::shadow`]: a
//! title buffer's copy, or the 4 MiB window of memory outside buffers), so
//! a root view and a raw address reach the same host bytes. This module
//! keeps what a shader needs to find them:
//!
//! - the **directory**: per 64 KiB page of title address space the number
//!   of the copy that holds the page (0: none) and the page's byte offset
//!   in it; one buffer for all lists, entries only ever go from empty to
//!   filled;
//! - per view heap a run of slots with a raw unordered access view of each
//!   numbered copy (slot 0 of the run: a null view, for pages without a
//!   copy);
//! - the **faults**: the page numbers shaders found empty, read back here
//!   once a frame, so the copies exist from the next frame on. A pointer a
//!   shader takes out of memory cannot be known before it runs; the
//!   addresses a dispatch is given as root views are made ready before it.
//!
//! **A page that is not there when a shader touches it is a lost access**:
//! the read gives 0, the store is dropped, and the fault note only helps
//! the next frame. For the structure builder that is the same as a dropped
//! stage, so before a dispatch the host makes ready everything it can see
//! the dispatch name: the whole copy around each root view, around each
//! pair of root constants that is a title address and around each root
//! constant that is one in 256-byte units (the builder passes structures
//! all three ways), and the 4 MiB after each (a structure may cross a
//! window's end).
//! shortcut: what a stage reaches only through pointers it reads out of
//! memory (InitHeaders' table of structure addresses, LoadInstances'
//! instance records) is not made ready unless something else named it;
//! the fault count in the log says whether that happens. The whole answer
//! is the extent of the title's allocation around an address, which the
//! memory module of the layer knows and this one does not ask yet.
//!
//! shortcut: a copy that is numbered is kept for the rest of the run, and
//! there are at most [`COPIES`] of them (4 MiB windows mostly: 4 GiB at
//! most). Every dispatch of such a shader brings all numbered copies up to
//! date and, if it can store, declares all of them written (the bytes are
//! compared on the CPU afterwards, as for any unordered access view): a
//! dirty-page mask written by the shader would bound both.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use victory_shader::emit::{MEMORY_FAULTS, MEMORY_PAGES};
use windows::Win32::Graphics::Direct3D12::*;

use crate::descriptor::host::HostHeap;
use crate::host::{HostList, host};
use crate::shadow::Shadow;

/// Bits 32 to 39 of what is taken for a title address among a dispatch's
/// constants and in the builder's tables: the card's addresses (0x7.. to
/// 0x10..) and the pool's views, not the processor's views of the same
/// pages below. A constant before a small number looked like one of those
/// (run bvh-42: a window at 0x130d90000 was numbered and guarded, and the
/// title's decompression into it stopped on the guard), and each numbered
/// a copy of pages that already had one.
const CARD_HIGH: std::ops::RangeInclusive<u32> = 0x07..=0x1f;

/// Copies that can be numbered (the run of heap slots is one longer). A
/// listed frame of the opening cutscene names about 50 windows.
const COPIES: u32 = 1023;

/// [`COPIES`] (with [`pages`], [`PAGE_COPIES`]), or what
/// `VICTORY_MEMORY_COPIES` says (up to 65535), read once.
fn copies_most() -> u32 {
    static MOST: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *MOST.get_or_init(|| {
        std::env::var("VICTORY_MEMORY_COPIES")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .map_or(if pages() { PAGE_COPIES } else { COPIES }, |n| n.clamp(1, 65535))
    })
}

/// Copies that can be numbered with [`pages`]: a page each, so many more.
const PAGE_COPIES: u32 = 65535;
/// A copy of view memory with [`pages`]: its page. (The translation looks up
/// each dword of an access on its own, so one that begins in a page's last
/// bytes continues in the next page's copy.)
const PAGE_COPY: u64 = PAGE;
/// How far from an address a dispatch names its stages are taken to reach
/// ([`pages`]): the 4 MiB a window was, twice.
const REACH: u64 = 2 * CELL;

/// `VICTORY_MEMORY_PAGES=1`: view memory (the title's physical pages, where
/// Grand Theft Auto VI streams its models) is held by address in copies of
/// one 64 KiB page, one per physical page however many addresses show it,
/// and a dispatch brings up to date and has compared only the copies near
/// what its arguments name. Memory that is no view, and title buffers, are
/// held as before.
///
/// As 4 MiB windows by address the structure builder's memory needed more
/// copies than can be numbered: all 1,023 were given out 1.1 seconds after
/// it started (run bvh-19), 596 of them to scattered pages of streamed
/// models, a window for a page, and every page after that read zeros; with
/// 4,095 it was 16.5 GB of windows at 0.1 fps (bvh-18), every dispatch
/// bringing all of them up to date and every submission comparing them.
///
/// The programs of the structure builder then reach their root views as
/// title memory by address too (`victory_shader::emit::set_memory_pages`):
/// a view bound as a buffer of its own would be other host bytes than the
/// page copies, and the stages feed each other through both.
///
/// shortcut: off unless set, until runs have shown it right.
/// shortcut: a copy another address names is brought up to date when it is
/// made and when a dispatch names memory near it, not by every dispatch: a
/// page the title rewrites in place that stages only reach through
/// pointers stays as it was. Stores through such pointers, more than
/// [`REACH`] from every named address, are not copied back into title
/// memory.
/// shortcut: copies of pages the title maps anew are not given up, nor
/// their numbers used again.
pub(crate) fn pages() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var_os("VICTORY_MEMORY_PAGES").is_some_and(|v| v == "1");
        victory_shader::emit::set_memory_pages(on);
        on
    })
}

/// Remappings told by the runtime and not yet taken in ([`remapped`]).
static REMAPPED: Mutex<Vec<(u64, u64)>> = Mutex::new(Vec::new());

/// The runtime mapped or unmapped title memory `[at, at + len)`: with
/// [`pages`] the directory's entries there name copies of what the pages
/// showed before. Noted here and taken in before the next dispatch (the
/// runtime holds its own locks when it tells).
pub(crate) fn remapped(at: usize, len: usize) {
    if !pages() || len == 0 {
        return;
    }
    let mut noted = REMAPPED.lock().unwrap_or_else(|e| e.into_inner());
    match noted.last_mut() {
        Some(last) if last.0 + last.1 == at as u64 => last.1 += len as u64,
        _ => noted.push((at as u64, len as u64)),
    }
}
/// A window of memory outside buffers (`shadow.rs`).
const CELL: u64 = 4 << 20;
/// A directory page.
const PAGE: u64 = 1 << 16;

/// The most megabytes of private memory the process may hold for another
/// copy to be numbered past the first [`COPIES`] (`VICTORY_MEMORY_MOST_MB`;
/// 34,000 by default: with the builder off the process peaks near that,
/// run bvh-49). With 4,095 numbers the structure builder and the
/// ray-tracing shaders had the process at 45 GB, 16.7 GB of it windows
/// numbered for them, and the PC at 6 GB free (run bvh-47). Pages past the
/// limit read zeros, as pages without a number do.
fn most_mb() -> u64 {
    static MOST: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *MOST.get_or_init(|| {
        std::env::var("VICTORY_MEMORY_MOST_MB").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(34_000)
    })
}

/// The process's private memory, in megabytes.
fn private_mb() -> u64 {
    use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut c = PROCESS_MEMORY_COUNTERS { cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32, ..Default::default() };
    // SAFETY: our own process and a sized structure.
    unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) };
    c.PagefileUsage as u64 >> 20
}

/// What the title asks the structure builder for in one call, while the
/// call runs on this thread (`XboxBvhBuilder.dll`'s
/// `BuildRaytracingAccelerationStructureBatched2X` and
/// `CopyRaytracingAccelerationStructure`, which the title's code calls and
/// `victory-hle` passes through here): the stretches of title memory the
/// call's dispatches read, and those they also store in.
///
/// With [`pages`] these are made ready, to the page, before the first
/// dispatch of the call, and nothing else is guessed for the builder's
/// dispatches: a structure is built once, and a stage that found a page
/// without a copy ran on zeros (run bvh-50: the first build stage missed
/// 278 pages in its first frame, and the title died 23 frames later on
/// what the stages stored from there). The descriptions say all of it: the
/// structure and the scratch memory with the sizes the builder itself
/// answers for the same inputs, each geometry's vertices, indices and
/// transform with their counts, the instances and the structures they
/// name.
struct Batch {
    /// `(address, bytes)`, stored in.
    writes: Vec<(u64, u64)>,
    /// `(address, bytes)`, only read.
    reads: Vec<(u64, u64)>,
    /// The lists whose submission already brings these up to date.
    lists: Vec<usize>,
}

thread_local! {
    static BATCH: std::cell::RefCell<Option<Batch>> = const { std::cell::RefCell::new(None) };
}

/// By address, a structure's bytes and the structures it names.
type Structures = std::collections::BTreeMap<u64, (u64, Vec<u64>)>;

/// The structures the builder was asked for: the bytes the builder said
/// each may take, and for one built over instances the structures those
/// name. The programs that walk a structure reach all of it through
/// pointers, from an acceleration structure view that only gives the
/// address.
static STRUCTURES: Mutex<Structures> = Mutex::new(Structures::new());

/// While it lives, the builder's dispatches on this thread are those of
/// one call ([`Batch`]).
pub struct Builds(());

impl Drop for Builds {
    fn drop(&mut self) {
        BATCH.with(|b| *b.borrow_mut() = None);
    }
}

/// A title address that can be read here: `len` bytes of it.
fn title_bytes(at: u64, len: usize) -> Option<&'static [u8]> {
    // SAFETY: just found committed; the title may be writing, and what is
    // read is taken as numbers only.
    (at != 0 && crate::texture::committed(at as *const u8, len)).then(|| unsafe { std::slice::from_raw_parts(at as *const u8, len) })
}

fn dword(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

fn qword(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

/// The bytes of the structure at `address`, at most `most`: what its
/// header says once it is built (the second dword is its last 64-byte
/// node; the leaves lie before it, run bvh-38's listing), so a structure
/// that turned out smaller than the builder's bound holds fewer pages.
fn structure_bytes(address: u64, most: u64) -> u64 {
    match title_bytes(address, 8) {
        Some(head) if dword(head, 0) & 0xfff0_ffff == 0x8000_0006 => ((u64::from(dword(head, 4)) + 1) * 64).min(most),
        _ => most,
    }
}

/// The bytes of a geometry's indices in the builder's packed format (53):
/// a 12-byte header (clusters in the low 17 bits of the first dword,
/// triangles in the second), 16 bytes a cluster, a dword a triangle (three
/// 7-bit corners and the cluster), then the clusters' 16-bit vertex
/// numbers, a cluster's from the offset its entry gives. Read off the
/// refit stage's code (5062bc7a48e842f8 from 0x1cc), which finds a corner
/// at `12 + 16 * clusters + 4 * triangles + 2 * (offset + corner)`.
fn packed_index_bytes(head: &[u8], table: &[u8]) -> u64 {
    let (clusters, triangles) = (u64::from(dword(head, 0) & 0x1_ffff), u64::from(dword(head, 4)));
    let last = table
        .chunks_exact(16)
        .map(|e| u64::from(dword(e, 0) << 2 & 0x1_0000 | dword(e, 4) & 0xffff))
        .max()
        .unwrap_or(0);
    12 + 16 * clusters + 4 * triangles + 2 * (last + 128)
}

/// [`packed_index_bytes`] of the indices at `at` in title memory.
fn packed_index_bytes_at(at: u64) -> u64 {
    let Some(head) = title_bytes(at, 12) else { return 12 };
    let clusters = (dword(head, 0) & 0x1_ffff) as usize;
    packed_index_bytes(head, title_bytes(at + 12, clusters * 16).unwrap_or(&[]))
}

/// What the build described by the 80 bytes of `desc` reads and stores
/// (`D3D12XBOX_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_BATCHED_DESC2`: the
/// PC's build description, 48 bytes; the count and array of post-build
/// notes; 16 bytes of stream description). `sizes` are the builder's own
/// bounds for these inputs: the structure, the scratch memory of a build
/// and of an update.
/// shortcut: a procedural geometry's boxes are taken as 24 bytes each
/// where the stride is smaller; no title seen has any.
fn build_ranges(desc: &[u8], sizes: [u64; 3], batch: &mut Batch, structures: &mut Structures) {
    let (dest, top, flags) = (qword(desc, 0), dword(desc, 8) == 0, dword(desc, 12));
    let (count, pointers, inputs) = (dword(desc, 16) as usize, dword(desc, 20) == 1, qword(desc, 24));
    let (source, scratch) = (qword(desc, 32), qword(desc, 40));
    let update = flags & 0x20 != 0;
    batch.writes.push((dest, sizes[0]));
    batch.writes.push((scratch, if update { sizes[2] } else { sizes[1] }));
    if update && source != 0 && source != dest {
        let most = structures.get(&source).map_or(sizes[0], |s| s.0);
        batch.reads.push((source, structure_bytes(source, most)));
    }
    if let Some(notes) = title_bytes(qword(desc, 56), dword(desc, 48) as usize * 16) {
        batch.writes.extend(notes.chunks_exact(16).map(|n| (qword(n, 0), 64)));
    }
    // Each description, in an array or behind an array of pointers.
    let each = |size: usize, i: usize| {
        if pointers {
            title_bytes(inputs + i as u64 * 8, 8).and_then(|p| title_bytes(qword(p, 0), size))
        } else {
            title_bytes(inputs + (i * size) as u64, size)
        }
    };
    let mut parts = Vec::new();
    for i in 0..count {
        if top {
            // An instance: its structure's address in its last 8 bytes.
            if pointers {
                batch.reads.push((inputs + i as u64 * 8, 8));
            }
            let Some(instance) = each(64, i) else { continue };
            batch.reads.push((instance.as_ptr() as u64, 64));
            let part = qword(instance, 56);
            if part != 0 && !parts.contains(&part) {
                parts.push(part);
                let most = structures.get(&part).map_or(0, |s| s.0);
                batch.reads.push((part, structure_bytes(part, most).max(256)));
            }
            continue;
        }
        let Some(g) = each(56, i) else { continue };
        if dword(g, 0) == 1 {
            batch.reads.push((qword(g, 16), qword(g, 8) * qword(g, 24).max(24)));
            continue;
        }
        let (transform, format) = (qword(g, 8), dword(g, 16));
        let (indices, vertices) = (u64::from(dword(g, 24)), u64::from(dword(g, 28)));
        if transform != 0 {
            batch.reads.push((transform, 48));
        }
        let index_at = qword(g, 32);
        if index_at != 0 {
            let bytes = match format {
                42 => indices * 4,
                53 => packed_index_bytes_at(index_at),
                _ => indices * 2,
            };
            batch.reads.push((index_at, bytes));
        }
        // 16 bytes past the last vertex's start cover every position
        // format, whatever the stride.
        batch.reads.push((qword(g, 40), vertices * qword(g, 48) + 16));
    }
    // What lay where the new structure goes is no structure any more.
    let gone: Vec<u64> = structures.range(dest..dest + sizes[0].max(1)).map(|(&a, _)| a).collect();
    for a in gone {
        structures.remove(&a);
    }
    structures.insert(dest, (sizes[0], parts));
}

/// The title asks the builder for the `count` builds described at `descs`
/// (80 bytes each, [`build_ranges`]); `sizes` are the builder's bounds for
/// each. Until the answer is dropped, the builder's dispatches on this
/// thread have exactly that memory made ready ([`Batch`]).
///
/// # Safety
/// `descs` points at `count` descriptions as the title passes them.
pub unsafe fn structure_builds(descs: *const u8, count: u32, sizes: &[[u64; 3]]) -> Builds {
    let mut batch = Batch { writes: Vec::new(), reads: Vec::new(), lists: Vec::new() };
    let mut structures = STRUCTURES.lock().unwrap_or_else(|e| e.into_inner());
    for (i, &sizes) in sizes.iter().enumerate().take(count as usize) {
        // SAFETY: the caller's descriptions.
        let desc = unsafe { std::slice::from_raw_parts(descs.add(i * 80), 80) };
        build_ranges(desc, sizes, &mut batch, &mut structures);
        #[cfg(feature = "diagnostics")]
        crate::pipeline::audit(|| {
            let words: Vec<u32> = (0..20).map(|k| dword(desc, k * 4)).collect();
            format!("build {words:08x?} sizes {sizes:x?}")
        });
    }
    drop(structures);
    #[cfg(feature = "diagnostics")]
    crate::pipeline::audit(|| {
        let bytes = |r: &[(u64, u64)]| r.iter().map(|r| r.1).sum::<u64>() >> 10;
        let (w, r) = (&batch.writes, &batch.reads);
        format!("builds {count} writes {} ({} KiB) reads {} ({} KiB)", w.len(), bytes(w), r.len(), bytes(r))
    });
    BATCH.with(|b| *b.borrow_mut() = Some(batch));
    Builds(())
}

/// The title asks the builder to copy the structure at `source` to
/// `dest`, compacted: as [`structure_builds`], for the copy's dispatches.
pub fn structure_copy(dest: u64, source: u64) -> Builds {
    let mut structures = STRUCTURES.lock().unwrap_or_else(|e| e.into_inner());
    let (most, parts) = structures.get(&source).cloned().unwrap_or((0, Vec::new()));
    // Its header is the title's by now: the title read the compacted size
    // back before it asked. Unknown here, the megabyte a window gave.
    let bytes = structure_bytes(source, if most == 0 { 1 << 20 } else { most });
    #[cfg(feature = "diagnostics")]
    crate::pipeline::audit(|| format!("copies {source:#x} to {dest:#x} bytes {bytes:#x} of {most:#x}"));
    structures.insert(dest, (bytes, parts));
    drop(structures);
    let batch = Batch { writes: vec![(dest, bytes)], reads: vec![(source, bytes)], lists: Vec::new() };
    BATCH.with(|b| *b.borrow_mut() = Some(batch));
    Builds(())
}

struct Memory {
    /// Whether the process holds more than [`most_mb`], as of this frame.
    full: bool,
    /// Buffer, its mapping, its GPU address.
    directory: (ID3D12Resource, *mut u32, u64),
    faults: (ID3D12Resource, *mut u32, u64),
    /// Copy `n` is `copies[n - 1]`.
    copies: Vec<Arc<Shadow>>,
    /// Per host view heap (by address): the first slot of its run, and how
    /// many copies have their view there.
    heaps: HashMap<usize, (u32, u32)>,
    /// The frame the faults were last read in.
    frame: u64,
    /// Pages asked for this frame that are not the title's memory.
    refused: std::collections::HashSet<u64>,
    /// With [`pages`]: the copy (its place in `copies`) of each physical
    /// page held (the section's handle, the offset in it).
    slices: HashMap<(usize, u64), u32>,
    /// With [`pages`]: the directory's entries that name page copies, by
    /// page number, with the copy.
    paged: std::collections::BTreeMap<u64, u32>,
    /// With [`pages`]: per page copy the physical page it stands for (none
    /// once its own address shows another) and the pages whose entries
    /// name it.
    held: HashMap<u32, (Option<(usize, u64)>, Vec<u64>)>,
    /// The copies that are no page copies (title buffers, windows).
    plain: Vec<u32>,
    /// With [`pages`]: page copies made and not yet brought up to date.
    fresh: Vec<u32>,
    /// The addresses the dispatch being made ready names.
    named: Vec<u64>,
    /// Of those, how many come first that its stages may store near: its
    /// root views and the address pairs among its constants. Not what a
    /// geometry table names (read only), nor a constant taken for an
    /// address in 256-byte units (any number of 0x0100_0000 or more is).
    stored: usize,
    /// With [`pages`]: the 4 MiB cells of memory past the directory that
    /// have a stand-in range in it ([`root_address`]), in the ranges' order.
    far: Vec<u64>,
}

/// The first page of the directory's stand-in ranges, its last 4 GiB, and
/// the pages of a range: a cell and the one after it.
const FAR_FIRST: u64 = MEMORY_PAGES as u64 - 0x1_0000;
const FAR_SPAN: u64 = 2 * CELL / PAGE;

/// With [`pages`], the address a program is given for a root view at
/// `address` that it reaches as title memory by address: the address
/// itself, or for memory the directory does not reach (the builder's own
/// tables lie in the host's heap, above 40 bits) an address in a stand-in
/// range of the directory that shows the 8 MiB from the address's 4 MiB
/// cell on. The program only ever adds offsets to it.
/// shortcut: there are 512 such ranges; when all are taken they are given
/// out again from the first, and what an earlier list was given for a
/// range may then show other memory (the builder's tables change with
/// every frame).
pub(crate) fn root_address(address: u64) -> u64 {
    if address / PAGE < FAR_FIRST {
        return address;
    }
    match MEMORY.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        Some(m) => m.stand_in(address),
        None => address,
    }
}

impl Memory {
    /// [`root_address`] of an address past the directory, its page and the
    /// next made ready.
    fn stand_in(&mut self, address: u64) -> u64 {
        let cell = address & !(CELL - 1);
        let k = match self.far.iter().position(|&c| c == cell) {
            Some(k) => k,
            None => {
                if self.far.len() as u64 >= (u64::from(MEMORY_PAGES) - 1 - FAR_FIRST) / FAR_SPAN {
                    // Every range is taken: their entries are emptied.
                    self.far.clear();
                    for page in FAR_FIRST..u64::from(MEMORY_PAGES) - 1 {
                        // SAFETY: inside the directory's mapping.
                        unsafe { self.directory.1.add(page as usize * 2).write_volatile(0) };
                    }
                }
                self.far.push(cell);
                self.far.len() - 1
            }
        };
        let stand_in = (FAR_FIRST + k as u64 * FAR_SPAN) * PAGE + (address - cell);
        // From the address on, as far as the range goes: the refit stage
        // read its table past the two pages made ready before (run bvh-40:
        // its faults at pages 0x1f2e0e, 0x1f2e2a, 0x1f2e37), and past the
        // megabyte made ready after that (run bvh-51: 400 to 1,100 pages a
        // frame, up to 30 pages into a range). The copies are windows of
        // 4 MiB, there with the first page.
        let end = (FAR_FIRST + (k as u64 + 1) * FAR_SPAN) * PAGE;
        let mut at = stand_in;
        while at < end {
            self.map(at);
            at += PAGE;
        }
        stand_in
    }
}

// SAFETY: the mappings are of buffers the struct owns; all access is under
// the mutex.
unsafe impl Send for Memory {}

static MEMORY: Mutex<Option<Memory>> = Mutex::new(None);

/// Diagnostics: the programs that read title memory by address, by the tag
/// they leave with a page they find empty (`victory_shader::emit::fault_tag`).
#[cfg(feature = "diagnostics")]
static FAULT_TAGS: Mutex<std::collections::BTreeMap<u32, Vec<String>>> = Mutex::new(std::collections::BTreeMap::new());

/// Notes the program `hash` with code `code` for the faults' report.
#[cfg(feature = "diagnostics")]
pub(crate) fn fault_program(code: &[u8], hash: &str) {
    let mut tags = FAULT_TAGS.lock().unwrap_or_else(|e| e.into_inner());
    let named = tags.entry(victory_shader::emit::fault_tag(code)).or_default();
    if !named.iter().any(|n| n == hash) {
        named.push(hash.to_owned());
    }
}

/// What a draw binds.
pub(crate) struct Ready {
    pub(crate) directory: u64,
    pub(crate) faults: u64,
    /// The view heap slot of the null view; copy `n`'s view is `n` further.
    pub(crate) base: u32,
}

impl Memory {
    fn new() -> Option<Memory> {
        let host = host()?;
        let make = |dwords: u64, what: &str| {
            let (buffer, data) = host.shared_buffer(dwords * 4)?;
            crate::host::name(&buffer, || format!("title memory by address: {what}"));
            // SAFETY: a fresh mapping of at least that many bytes.
            unsafe { std::ptr::write_bytes(data, 0, (dwords * 4) as usize) };
            // SAFETY: a live buffer.
            let gpu = unsafe { buffer.GetGPUVirtualAddress() };
            Some((buffer, data.cast::<u32>(), gpu))
        };
        tracing::info!("title memory by address: a shader reads or writes through raw addresses");
        Some(Memory {
            directory: make(u64::from(MEMORY_PAGES) * 2, "directory")?,
            faults: make(u64::from(MEMORY_FAULTS), "faults")?,
            copies: Vec::new(),
            heaps: HashMap::new(),
            frame: u64::MAX,
            full: false,
            refused: Default::default(),
            slices: HashMap::new(),
            paged: Default::default(),
            held: HashMap::new(),
            plain: Vec::new(),
            fresh: Vec::new(),
            named: Vec::new(),
            stored: 0,
            far: Vec::new(),
        })
    }

    /// Gives the page of `address` its copy, if it has none and the memory
    /// is the title's.
    fn map(&mut self, address: u64) {
        let page = address / PAGE;
        // The last entry stays empty: addresses past the directory read it.
        if page >= u64::from(MEMORY_PAGES) - 1 {
            return;
        }
        // SAFETY: inside the directory's mapping.
        let entry = unsafe { self.directory.1.add(page as usize * 2) };
        // SAFETY: as above.
        if unsafe { entry.read() } != 0 {
            return;
        }
        // A page of the stand-in range shows memory past the directory
        // (`root_address`).
        let far = page >= FAR_FIRST;
        let start = if far {
            let Some(&cell) = self.far.get(((page - FAR_FIRST) / FAR_SPAN) as usize) else {
                return;
            };
            cell + (page - FAR_FIRST) % FAR_SPAN * PAGE
        } else {
            page * PAGE
        };
        // A shader's pointer, or a constant taken for an address, may be
        // anything: only memory the title has.
        if self.refused.contains(&page) {
            return;
        }
        // Diagnostics: why a page gets no copy.
        #[cfg(feature = "diagnostics")]
        let why = |what: &str| crate::pipeline::audit(|| format!("nocopy {page:x} {what}"));
        #[cfg(not(feature = "diagnostics"))]
        let why = |_: &str| {};
        if !crate::texture::committed(start as *const u8, PAGE as usize) {
            self.refused.insert(page);
            why("not committed");
            return;
        }
        // View memory outside title buffers, with `pages`: the copy of the
        // physical page, whichever address asked first.
        if pages()
            && !far
            && crate::shadow::extent(start).is_none()
            && let Some(slice) = crate::inplace::slice_of(start as usize)
        {
            let n = match self.slices.get(&slice) {
                Some(&n) => n,
                None if self.full && self.copies.len() >= COPIES as usize => {
                    why("the process holds too much memory");
                    return;
                }
                None if self.copies.len() < copies_most() as usize => {
                    let Some(shadow) = crate::shadow::window_at(start, PAGE_COPY) else {
                        why("no copy could be made");
                        return;
                    };
                    self.copies.push(shadow);
                    let n = self.copies.len() as u32 - 1;
                    self.slices.insert(slice, n);
                    self.held.insert(n, (Some(slice), Vec::new()));
                    self.fresh.push(n);
                    n
                }
                None => {
                    static ONCE: std::sync::Once = std::sync::Once::new();
                    ONCE.call_once(|| {
                        tracing::warn!(most = copies_most(), "title memory by address: no number left for another page's copy; its pages read zeros")
                    });
                    why("no number left");
                    return;
                }
            };
            self.paged.insert(page, n);
            if let Some((_, named)) = self.held.get_mut(&n) {
                named.push(page);
            }
            // The offset first: a shader that sees the number sees it too.
            // SAFETY: inside the directory's mapping.
            unsafe {
                entry.add(1).write_volatile(0);
                entry.write_volatile(n + 1);
            }
            return;
        }
        // Past the limit no window is made to be numbered (run bvh-48: the
        // numbering was refused after the window was there, and the process
        // went on to 40.5 GB). A page of a window that is there already is
        // refused with it: only buffers are known without making a copy.
        if self.full && self.copies.len() >= COPIES as usize && crate::shadow::extent(start).is_none() {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| {
                tracing::warn!(most_mb = most_mb(), "title memory by address: the process holds too much memory for another window; its pages read zeros")
            });
            why("the process holds too much memory");
            return;
        }
        // The copy a root view at that address would get; a buffer that
        // ends inside the page is passed over for what holds all of it.
        let whole = |s: &Arc<Shadow>| s.offset_in(start).is_some_and(|_| s.left(start) >= PAGE);
        let Some(shadow) = crate::shadow::find(start, 1)
            .filter(whole)
            .or_else(|| crate::shadow::find(start, PAGE).filter(whole))
        else {
            why("no copy holds the whole page");
            return;
        };
        // SAFETY: a live buffer.
        let flags = unsafe { shadow.buffer.GetDesc() }.Flags;
        if flags.0 & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 == 0 {
            why("its copy takes no unordered access");
            return;
        }
        let n = match self.copies.iter().position(|c| Arc::ptr_eq(c, &shadow)) {
            Some(n) => n,
            None if self.full && self.copies.len() >= COPIES as usize => {
                static ONCE: std::sync::Once = std::sync::Once::new();
                ONCE.call_once(|| {
                    tracing::warn!(most_mb = most_mb(), "title memory by address: the process holds too much memory for another copy; its pages read zeros")
                });
                why("the process holds too much memory");
                return;
            }
            None if self.copies.len() < copies_most() as usize => {
                self.copies.push(shadow.clone());
                let n = self.copies.len() - 1;
                self.plain.push(n as u32);
                #[cfg(feature = "diagnostics")]
                crate::pipeline::audit(|| format!("copy {n} {:#x}+{:#x} for page {page:x}", shadow.span().0, shadow.span().1));
                // Every page the copy holds whole, not only the one asked
                // for: a structure is more than 64 KiB.
                // shortcut: a page of a title buffer inside this one that
                // has a copy of its own is given this copy too.
                let (first, len) = shadow.span();
                let mut at = first.next_multiple_of(PAGE);
                while at + PAGE <= first + len && at / PAGE < u64::from(MEMORY_PAGES) - 1 {
                    // SAFETY: inside the directory's mapping.
                    let other = unsafe { self.directory.1.add((at / PAGE) as usize * 2) };
                    // SAFETY: as above; the offset first, as below.
                    unsafe {
                        if other.read() == 0 {
                            other.add(1).write_volatile((at - first) as u32);
                            other.write_volatile(n as u32 + 1);
                        }
                    }
                    at += PAGE;
                }
                n
            }
            None => {
                static ONCE: std::sync::Once = std::sync::Once::new();
                ONCE.call_once(|| {
                    tracing::warn!(COPIES, "title memory by address: no number left for another copy; its pages read zeros")
                });
                why("no number left");
                return;
            }
        };
        let offset = shadow.offset_in(start).unwrap_or(0);
        // The offset first: a shader that sees the number sees it too.
        // SAFETY: inside the directory's mapping.
        unsafe {
            entry.add(1).write_volatile(offset as u32);
            entry.write_volatile(n as u32 + 1);
        }
    }

    /// The pages shaders found empty since the last frame.
    fn take_faults(&mut self) {
        let mut pages = Vec::new();
        #[cfg(feature = "diagnostics")]
        let mut tagged: Vec<(u32, u32)> = Vec::new();
        for i in 1..MEMORY_FAULTS as usize {
            // SAFETY: inside the faults' mapping; the GPU may be writing,
            // a number missed now is written again next frame.
            let at = unsafe { self.faults.1.add(i) };
            // SAFETY: as above.
            let page = unsafe { at.read_volatile() };
            if page != 0 {
                #[cfg(feature = "diagnostics")]
                tagged.push((page >> 21, page & (MEMORY_PAGES - 1)));
                let page = page & (MEMORY_PAGES - 1);
                pages.push(page);
                // SAFETY: as above.
                unsafe { at.write_volatile(0) };
            }
        }
        if !pages.is_empty() {
            // Each is an access that read 0 or was dropped.
            static SAID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            if SAID.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 50 {
                tracing::warn!(
                    pages = pages.len(),
                    first = format_args!("{:#x}", u64::from(pages[0]) * PAGE),
                    "title memory by address: shaders touched pages that had no copy (those accesses read 0 or were dropped); the copies are made now"
                );
            }
        }
        // Which programs touched them (their tags, `fault_tag`), most pages first.
        #[cfg(feature = "diagnostics")]
        crate::pipeline::audit(|| {
            let mut by: std::collections::BTreeMap<u32, Vec<u32>> = Default::default();
            for &(tag, page) in &tagged {
                by.entry(tag).or_default().push(page);
            }
            let mut by: Vec<_> = by.into_iter().collect();
            by.sort_by_key(|(_, pages)| std::cmp::Reverse(pages.len()));
            let names = FAULT_TAGS.lock().unwrap_or_else(|e| e.into_inner());
            let said: Vec<String> = by
                .iter()
                .map(|(tag, pages)| format!("{tag:x} {:?} {} {:x?}", names.get(tag), pages.len(), &pages[..pages.len().min(12)]))
                .collect();
            format!("faults {} | {}", tagged.len(), said.join(" | "))
        });
        for page in pages {
            self.map(u64::from(page) * PAGE);
        }
    }

    /// A root view above the title's 40 bits is a table the structure
    /// builder's own code keeps in the host's heap: per geometry the
    /// addresses of its vertices, its indices and its transform, which the
    /// stage then reads by address. Those are named too, so that their
    /// copies are there when the stage runs. Without it the first build of
    /// a structure read zeros for every vertex (run bvh-11: 263, 54 and 47
    /// pages in the builder's first three frames, all touched by the stage
    /// that loads triangles), and a structure is built once.
    /// shortcut: the table's length is not known: its first 16 KiB are
    /// looked through for address pairs on 8 bytes (low dword, then bits 32
    /// to 39), which other data there may look like; what is not the title's
    /// memory is refused.
    fn name_table(&mut self, table: u64) {
        const LOOKED: usize = 16 << 10;
        if table >> 40 == 0 || !crate::texture::committed(table as *const u8, LOOKED) {
            return;
        }
        // SAFETY: just found committed; the builder's code may be writing
        // it, and any value read is looked at as a number only.
        let pairs = unsafe { std::slice::from_raw_parts(table as *const [u32; 2], LOOKED / 8) };
        for &[low, high] in pairs {
            if CARD_HIGH.contains(&high) && low >= 0x1_0000 {
                let address = u64::from(low) | u64::from(high) << 32;
                self.name(address);
                self.named.push(address);
            }
        }
    }

    /// What a dispatch names: the copy around `address` and the 4 MiB
    /// after it.
    fn name(&mut self, address: u64) {
        // With `pages`, in view memory: the pages from the address to where
        // the views that follow one another end (a streamed resource).
        if pages() && crate::inplace::slice_of(address as usize).is_some() {
            let end = crate::inplace::viewed_to(address as usize, REACH as usize) as u64;
            let mut at = address & !(PAGE - 1);
            while at < end {
                self.map(at);
                at += PAGE;
            }
            return;
        }
        self.map(address);
        self.map((address & !(CELL - 1)) + CELL);
    }

    /// Takes in what the runtime mapped anew ([`remapped`]): the entries of
    /// those pages are emptied, and a copy whose own address is among them
    /// no longer stands for its physical page.
    fn take_remapped(&mut self) {
        let noted = std::mem::take(&mut *REMAPPED.lock().unwrap_or_else(|e| e.into_inner()));
        for (at, len) in noted {
            let range = at / PAGE..(at + len).div_ceil(PAGE);
            let gone: Vec<(u64, u32)> = self.paged.range(range.clone()).map(|(&page, &n)| (page, n)).collect();
            for (page, n) in gone {
                self.paged.remove(&page);
                // SAFETY: inside the directory's mapping (the page had an entry).
                unsafe { self.directory.1.add(page as usize * 2).write_volatile(0) };
                let start = self.copies[n as usize].span().0;
                let Some((slice, named)) = self.held.get_mut(&n) else { continue };
                named.retain(|&p| p != page);
                if range.contains(&(start / PAGE)) {
                    if let Some(slice) = slice.take() {
                        self.slices.remove(&slice);
                    }
                    // Its other addresses find the page's new copy.
                    for other in std::mem::take(named) {
                        self.paged.remove(&other);
                        // SAFETY: as above.
                        unsafe { self.directory.1.add(other as usize * 2).write_volatile(0) };
                    }
                }
            }
        }
    }
}

/// Before a draw whose shader reads title memory by address: what its
/// root arguments name gets its copies (`addresses`: its root views;
/// `constants`: its root constants, see the module), every numbered copy
/// has its view in `heap`'s run and is brought up to date when `list` is
/// submitted (and, with `writes`, copied back after). None if the host
/// cannot hold it; the draw is then dropped.
pub(crate) fn ready(
    list: &HostList,
    heap: &HostHeap,
    addresses: &[u64],
    constants: &[u32],
    structures: &[u64],
    writes: bool,
) -> Option<Ready> {
    let mut guard = MEMORY.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        *guard = Memory::new();
    }
    let m = guard.as_mut()?;
    let frame = crate::frame::current();
    if m.frame != frame {
        m.frame = frame;
        m.refused.clear();
        m.full = private_mb() > most_mb();
        #[cfg(feature = "diagnostics")]
        crate::pipeline::audit(|| format!("held {} copies, {} of them pages; {} MB, full {}", m.copies.len(), m.held.len(), private_mb(), m.full));
        m.take_faults();
    }
    m.take_remapped();
    let numbered = m.copies.len();
    m.named.clear();
    // The dispatches of a call of the structure builder have what the call
    // describes, and no more ([`Batch`]).
    let batched = pages() && BATCH.with(|b| b.borrow().is_some());
    for &address in addresses {
        if pages() && address / PAGE >= FAR_FIRST {
            // Reached through its stand-in range (`root_address`). Named, as
            // without `pages`: the windows there are the builder's tables,
            // which it writes anew for every call, and only what a dispatch
            // names is brought up to date once a list (with more than
            // `COPIES` numbered); they were brought up to date only when made.
            m.stand_in(address);
            m.named.push(address);
        } else if batched {
            m.map(address);
        } else {
            m.name(address);
            m.named.push(address);
        }
    }
    let (constants, structures) = if batched { (&[][..], &[][..]) } else { (constants, structures) };
    // A title address in two constants (low dword, then bits 32 to 39), or
    // in one in 256-byte units. A constant that only looks like one names
    // memory nothing reads; one that is not the title's memory is refused.
    for (i, &c) in constants.iter().enumerate() {
        if let Some(&high) = constants.get(i + 1)
            && CARD_HIGH.contains(&high)
            && c >= 0x1_0000
        {
            let address = u64::from(c) | u64::from(high) << 32;
            m.name(address);
            m.named.push(address);
        }
    }
    m.stored = m.named.len();
    // The structures its acceleration structure views name, as long as
    // their headers say: the second dword is the last 64-byte node.
    for &address in structures {
        m.name(address);
        m.named.push(address);
        // One the builder was asked for, with the structures its instances
        // name: every page of each, which a walk reaches through pointers
        // (run bvh-50: the two programs that walk them found 700 to 3,500
        // pages without a copy in most frames).
        if pages() {
            let known = STRUCTURES.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((most, parts)) = known.get(&address) {
                let part = |&a: &u64| (a, known.get(&a).map_or(0, |s| s.0));
                for (at, most) in std::iter::once((address, *most)).chain(parts.iter().map(part)) {
                    for page in at / PAGE..(at + structure_bytes(at, most)).div_ceil(PAGE) {
                        m.map(page * PAGE);
                    }
                }
                continue;
            }
        }
        if address >> 40 != 0 || !crate::texture::committed(address as *const u8, 8) {
            continue;
        }
        // SAFETY: just found committed; any value read is a number only.
        let head = unsafe { (address as *const [u32; 2]).read_unaligned() };
        if head[0] & 0xfff0_ffff != 0x8000_0006 {
            continue;
        }
        let end = address + (u64::from(head[1]) + 1).min(1 << 20) * 64;
        let mut at = (address & !(PAGE - 1)) + PAGE;
        while at < end {
            m.map(at);
            at += PAGE;
        }
    }
    // Only where the card's addresses of title memory are (from 0x7 to
    // below 0xe in bits 32 to 39): a float or a packed colour shifted by
    // 8 bits lands in the pool's views above, which are committed, and
    // each such guess numbered copies (run bvh-41, with `pages`: 16,838 of
    // 60,048 page copies, most at 0xe2.., 0xf1.. and 0xf5..).
    for &c in constants {
        if (0x0700_0000..0x0e00_0000).contains(&c) {
            m.name(u64::from(c) << 8);
            m.named.push(u64::from(c) << 8);
        }
    }
    if !batched {
        for &address in addresses {
            m.name_table(address);
        }
    }
    // Each stretch of the call, once a list: its pages get their copies,
    // and the copies are the ones brought up to date and, where the call
    // stores, compared afterwards.
    let mut near: Vec<(u32, bool)> = Vec::new();
    if batched {
        BATCH.with(|b| {
            let mut b = b.borrow_mut();
            let Some(b) = b.as_mut().filter(|b| !b.lists.contains(&(std::ptr::from_ref(list) as usize))) else {
                return;
            };
            b.lists.push(std::ptr::from_ref(list) as usize);
            for (ranges, stored) in [(&b.writes, true), (&b.reads, false)] {
                for &(at, len) in ranges.iter().filter(|r| r.0 != 0 && r.1 != 0) {
                    #[cfg(feature = "diagnostics")]
                    crate::pipeline::audit(|| format!("range {at:#x} {len:#x} {stored}"));
                    for page in at / PAGE..(at + len).div_ceil(PAGE) {
                        m.map(page * PAGE);
                        match m.paged.get(&page) {
                            Some(&n) => near.push((n, stored)),
                            // A title buffer's copy or a window: by the
                            // names, as without a call.
                            None => m.named.push(page * PAGE),
                        }
                    }
                }
                if stored {
                    m.stored = m.named.len();
                }
            }
        });
    }
    let key = std::ptr::from_ref(heap) as usize;
    let (base, made) = match m.heaps.get(&key) {
        Some(&run) => run,
        None => {
            // Held for the heap's life: not given back with a list's slots.
            let base = heap.dynamic(copies_most() + 1)?;
            heap.null_raw_uav_at(base);
            (base, 0)
        }
    };
    for (n, shadow) in m.copies.iter().enumerate().skip(made as usize) {
        let (_, len) = shadow.span();
        heap.raw_uav_at(base + 1 + n as u32, &shadow.buffer, 0, len);
    }
    m.heaps.insert(key, (base, m.copies.len() as u32));
    // Once per list, and for copies numbered since: the builder alone
    // dispatches hundreds of times a frame.
    // shortcut: a copy another list numbers meanwhile is brought up to date
    // by that list's submission, which may run after this one's.
    const READ: usize = usize::MAX - 0x6d;
    let first_read = list.first_use(READ);
    /// Keys of [`HostList::first_use`] for a plain copy's writes with `pages`.
    const NEAR_PLAIN: usize = usize::MAX - 0x80_0000;
    // With `pages`: the page copies near what the dispatch names, and those
    // made since the last dispatch; the other copies as before.
    near.extend(std::mem::take(&mut m.fresh).into_iter().map(|n| (n, false)));
    for (i, &address) in m.named.iter().enumerate().filter(|_| !batched) {
        let page = address / PAGE;
        near.extend(m.paged.range(page..=page + REACH / PAGE).map(|(_, &n)| (n, i < m.stored)));
    }
    // Each once, stored near if any of its names says so.
    near.sort_unstable_by_key(|&(n, stored)| (n, !stored));
    near.dedup_by_key(|&mut (n, _)| n);
    for &n in &m.plain {
        let (n, shadow) = (n as usize, &m.copies[n as usize]);
        let (start, len) = shadow.span();
        // Past the first `COPIES`, only the copies the dispatch names
        // memory in or just before, once a list each, and new ones: with
        // 4,095 numbered, bringing every one up to date for every list
        // was most of a second a frame (run bvh-47).
        // shortcut: a copy reached only through pointers is then as the
        // last list that named or drew from it left it.
        let all = m.copies.len() <= COPIES as usize;
        let near = || m.named.iter().any(|&a| a < start + len && a + REACH > start);
        if n >= numbered || if all { first_read } else { near() && list.first_use(NEAR_PLAIN - 0x1_0000 - n) } {
            shadow.refresh(list, start, len);
        }
        // Not memory past the title's 40 bits (the builder's tables in the
        // host's heap, which its stages only read): what the compare took
        // for a shader's store there would go into the heap of this very
        // process.
        // Only where the dispatch names memory in or just before the
        // copy: a stage that stores through a garbage pointer (run bvh-26:
        // 75 KiB of node headers in a window of the title's heap that a
        // fault had numbered) then leaves title memory alone, and the
        // copies a dispatch does not name are not kept and compared for
        // it (every numbered copy was, once a list: with 4,095 numbers
        // that is 16 GB a submission, run bvh-20's 0.1 frames a second).
        // shortcut: what a stage stores through a pointer into a copy no
        // dispatch of the list names stays in the host's copy; the title
        // does not see it.
        let named = m.named[..m.stored].iter().any(|&a| a < start + len && a + REACH > start);
        if writes && named && start >> 40 == 0 && list.first_use(NEAR_PLAIN - n) {
            shadow.gpu_writes(list, start, len);
        }
    }
    for (n, stored) in near {
        // Once per list each, as the others.
        const NEAR: usize = usize::MAX - 0x100_0000;
        let shadow = &m.copies[n as usize];
        let (start, len) = shadow.span();
        if list.first_use(NEAR - 2 * n as usize) {
            shadow.refresh(list, start, len);
        }
        if writes && stored && list.first_use(NEAR - 2 * n as usize - 1) {
            shadow.gpu_writes(list, start, len);
        }
    }
    if first_read {
        list.keep(&m.directory.0);
        list.keep(&m.faults.0);
    }
    Some(Ready {
        directory: m.directory.2,
        faults: m.faults.2,
        base,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(bytes: &mut [u8], at: usize, v: u64, len: usize) {
        bytes[at..at + len].copy_from_slice(&v.to_le_bytes()[..len]);
    }

    /// Two clusters and five triangles; the second cluster's vertex
    /// numbers begin 0x1_0003 words in (bit 14 of its first dword, the low
    /// word of its second).
    #[test]
    fn packed_indices_end_after_the_last_clusters_numbers() {
        let mut head = [0u8; 12];
        put(&mut head, 0, 0xfff0_0002, 4);
        put(&mut head, 4, 5, 4);
        let mut table = [0u8; 32];
        put(&mut table, 4, 0x9999_0040, 4);
        put(&mut table, 16, 1 << 14, 4);
        put(&mut table, 20, 3, 4);
        assert_eq!(packed_index_bytes(&head, &table), 12 + 32 + 20 + 2 * (0x1_0003 + 128));
    }

    /// A bottom-level build of two geometries, then a top-level update
    /// over two instances of it: what each reads and stores.
    #[test]
    fn a_build_names_what_its_description_says() {
        let transform = [0u8; 48];
        let mut geometry = [0u8; 112];
        // 300 16-bit indices at 0x7_1000_0000, 100 vertices of 12 bytes.
        put(&mut geometry, 8, transform.as_ptr() as u64, 8);
        put(&mut geometry, 16, 57, 4);
        put(&mut geometry, 24, 300, 4);
        put(&mut geometry, 28, 100, 4);
        put(&mut geometry, 32, 0x7_1000_0000, 8);
        put(&mut geometry, 40, 0x7_2000_0000, 8);
        put(&mut geometry, 48, 12, 8);
        // 30 32-bit indices, 10 vertices of 32 bytes, no transform.
        put(&mut geometry, 56 + 16, 42, 4);
        put(&mut geometry, 56 + 24, 30, 4);
        put(&mut geometry, 56 + 28, 10, 4);
        put(&mut geometry, 56 + 32, 0x7_3000_0000, 8);
        put(&mut geometry, 56 + 40, 0x7_4000_0000, 8);
        put(&mut geometry, 56 + 48, 32, 8);
        let note = [0x7_5000_0000u64, 0];
        let mut desc = [0u8; 80];
        put(&mut desc, 0, 0x8_0000_0000, 8);
        put(&mut desc, 8, 1, 4);
        put(&mut desc, 16, 2, 4);
        put(&mut desc, 24, geometry.as_ptr() as u64, 8);
        put(&mut desc, 40, 0x9_0000_0000, 8);
        put(&mut desc, 48, 1, 4);
        put(&mut desc, 56, note.as_ptr() as u64, 8);
        let mut batch = Batch { writes: Vec::new(), reads: Vec::new(), lists: Vec::new() };
        let mut structures = Structures::new();
        // A structure that lay inside the new one's bytes is forgotten.
        structures.insert(0x8_0000_1000, (64, Vec::new()));
        build_ranges(&desc, [0x3000, 0x5000, 0x800], &mut batch, &mut structures);
        assert_eq!(batch.writes, [(0x8_0000_0000, 0x3000), (0x9_0000_0000, 0x5000), (0x7_5000_0000, 64)]);
        assert_eq!(
            batch.reads,
            [
                (transform.as_ptr() as u64, 48),
                (0x7_1000_0000, 600),
                (0x7_2000_0000, 1216),
                (0x7_3000_0000, 120),
                (0x7_4000_0000, 336)
            ]
        );
        assert_eq!(structures.into_iter().collect::<Vec<_>>(), [(0x8_0000_0000, (0x3000, Vec::new()))]);

        let mut instances = [0u8; 128];
        put(&mut instances, 56, 0x8_0000_0000, 8);
        put(&mut instances, 64 + 56, 0x8_0000_0000, 8);
        let mut desc = [0u8; 80];
        put(&mut desc, 0, 0xa_0000_0000, 8);
        put(&mut desc, 12, 0x20, 4);
        put(&mut desc, 16, 2, 4);
        put(&mut desc, 24, instances.as_ptr() as u64, 8);
        put(&mut desc, 32, 0xb_0000_0000, 8);
        put(&mut desc, 40, 0x9_0000_0000, 8);
        let mut batch = Batch { writes: Vec::new(), reads: Vec::new(), lists: Vec::new() };
        let mut structures = Structures::from([(0x8_0000_0000, (0x3000, Vec::new())), (0xb_0000_0000, (0x700, Vec::new()))]);
        build_ranges(&desc, [0x1000, 0x5000, 0x800], &mut batch, &mut structures);
        // The scratch memory of an update, and the structure updated from.
        assert_eq!(batch.writes, [(0xa_0000_0000, 0x1000), (0x9_0000_0000, 0x800)]);
        let at = instances.as_ptr() as u64;
        assert_eq!(batch.reads, [(0xb_0000_0000, 0x700), (at, 64), (0x8_0000_0000, 0x3000), (at + 64, 64)]);
        assert_eq!(structures[&0xa_0000_0000], (0x1000, vec![0x8_0000_0000]));
    }
}
