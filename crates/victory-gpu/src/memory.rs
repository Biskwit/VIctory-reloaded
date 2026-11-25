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
/// A copy of view memory with [`pages`]: its page, and the page after it
/// for an access that begins in the page's last bytes.
const PAGE_COPY: u64 = 2 * PAGE;
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
/// shortcut: a store that begins in a page's last bytes and ends in the
/// next page lands in the first page's copy only (the builder's nodes lie
/// on 64 bytes). Copies of pages the title maps anew are not given up, nor
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
        // The megabyte from the address on, as far as the range goes: the
        // refit stage read its table past the two pages made ready before
        // (run bvh-40: its faults at pages 0x1f2e0e, 0x1f2e2a, 0x1f2e37).
        let end = (FAR_FIRST + (k as u64 + 1) * FAR_SPAN) * PAGE;
        let mut at = stand_in;
        while at < (stand_in + 16 * PAGE).min(end) {
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
        m.take_faults();
    }
    m.take_remapped();
    let numbered = m.copies.len();
    m.named.clear();
    for &address in addresses {
        if pages() && address / PAGE >= FAR_FIRST {
            // Reached through its stand-in range (`root_address`).
            m.stand_in(address);
        } else {
            m.name(address);
            m.named.push(address);
        }
    }
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
    for &address in addresses {
        m.name_table(address);
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
    let mut near: Vec<(u32, bool)> = std::mem::take(&mut m.fresh).into_iter().map(|n| (n, false)).collect();
    for (i, &address) in m.named.iter().enumerate() {
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
