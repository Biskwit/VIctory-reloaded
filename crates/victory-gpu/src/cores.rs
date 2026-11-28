//! Which processors our own threads run on, where a processor has two kinds
//! of cores. `cpu_sets` and `keep_on_fast_cores` are fxceless/victory-fork's
//! (`framelog.rs`, commit 0b0074e0, GPL-3.0-or-later, as this).

use std::sync::atomic::{AtomicU8, AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};

/// The system's processor sets of group 0: each one's id, its efficiency
/// class (higher is faster), its core (which two of them may share) and its
/// logical processor's number.
fn cpu_set_records() -> Vec<(u32, u8, u8, u8)> {
    use windows_sys::Win32::System::SystemInformation::{GetSystemCpuSetInformation, SYSTEM_CPU_SET_INFORMATION};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut bytes = 0u32;
    // SAFETY: a size query, then the call into a buffer of that size.
    unsafe { GetSystemCpuSetInformation(std::ptr::null_mut(), 0, &mut bytes, GetCurrentProcess(), 0) };
    let mut buffer = vec![0u64; (bytes as usize).div_ceil(8).max(1)];
    // SAFETY: as above; the buffer holds `bytes` bytes, aligned for the records.
    let ok = unsafe { GetSystemCpuSetInformation(buffer.as_mut_ptr().cast(), bytes, &mut bytes, GetCurrentProcess(), 0) };
    let mut sets = Vec::new();
    let mut at = 0usize;
    while ok != 0 && at + size_of::<SYSTEM_CPU_SET_INFORMATION>() <= bytes as usize {
        // SAFETY: a record inside the buffer; each says its own size.
        let info = unsafe { &*(buffer.as_ptr().cast::<u8>().add(at).cast::<SYSTEM_CPU_SET_INFORMATION>()) };
        if info.Size == 0 {
            break;
        }
        // SAFETY: the record's type is CpuSet (the only one there is).
        let set = unsafe { &info.Anonymous.CpuSet };
        if set.Group == 0 {
            sets.push((set.Id, set.EfficiencyClass, set.CoreIndex, set.LogicalProcessorIndex));
        }
        at += info.Size as usize;
    }
    sets
}

/// The sets by core: each one's id, its efficiency class and its core.
#[cfg(test)]
fn cpu_sets() -> Vec<(u32, u8, u8)> {
    cpu_set_records().into_iter().map(|(id, class, core, _)| (id, class, core)).collect()
}

/// The bytes of the last-level cache each processor set of group 0 shares,
/// by set id (none known: empty).
fn l3_of_sets() -> Vec<(u32, u32)> {
    use windows_sys::Win32::System::SystemInformation::{
        GetLogicalProcessorInformationEx, GetSystemCpuSetInformation, RelationCache, SYSTEM_CPU_SET_INFORMATION,
        SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // The largest cache of each logical processor of group 0, by its index.
    let mut by_processor = [0u32; 64];
    let mut bytes = 0u32;
    // SAFETY: a size query, then the call into a buffer of that size.
    unsafe { GetLogicalProcessorInformationEx(RelationCache, std::ptr::null_mut(), &mut bytes) };
    let mut buffer = vec![0u64; (bytes as usize).div_ceil(8).max(1)];
    // SAFETY: as above; the buffer holds `bytes` bytes, aligned for the records.
    let ok = unsafe { GetLogicalProcessorInformationEx(RelationCache, buffer.as_mut_ptr().cast(), &mut bytes) };
    let mut at = 0usize;
    while ok != 0 && at + size_of::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX>() <= bytes as usize {
        // SAFETY: a record inside the buffer; each says its own size.
        let info = unsafe { &*(buffer.as_ptr().cast::<u8>().add(at).cast::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX>()) };
        if info.Size == 0 {
            break;
        }
        if info.Relationship == RelationCache {
            // SAFETY: a cache record (asked for), with at least its first group mask.
            let cache = unsafe { &info.Anonymous.Cache };
            let mask = unsafe { cache.Anonymous.GroupMask };
            if mask.Group == 0 {
                for (p, size) in by_processor.iter_mut().enumerate() {
                    if mask.Mask >> p & 1 != 0 {
                        *size = (*size).max(cache.CacheSize);
                    }
                }
            }
        }
        at += info.Size as usize;
    }
    // Processor indices to set ids.
    let mut bytes = 0u32;
    // SAFETY: as for `cpu_sets`.
    unsafe { GetSystemCpuSetInformation(std::ptr::null_mut(), 0, &mut bytes, GetCurrentProcess(), 0) };
    let mut buffer = vec![0u64; (bytes as usize).div_ceil(8).max(1)];
    // SAFETY: as above.
    let ok = unsafe { GetSystemCpuSetInformation(buffer.as_mut_ptr().cast(), bytes, &mut bytes, GetCurrentProcess(), 0) };
    let mut sizes = Vec::new();
    let mut at = 0usize;
    while ok != 0 && at + size_of::<SYSTEM_CPU_SET_INFORMATION>() <= bytes as usize {
        // SAFETY: a record inside the buffer; each says its own size.
        let info = unsafe { &*(buffer.as_ptr().cast::<u8>().add(at).cast::<SYSTEM_CPU_SET_INFORMATION>()) };
        if info.Size == 0 {
            break;
        }
        // SAFETY: the record's type is CpuSet (the only one there is).
        let set = unsafe { &info.Anonymous.CpuSet };
        if set.Group == 0 && let Some(&size) = by_processor.get(set.LogicalProcessorIndex as usize) {
            sizes.push((set.Id, size));
        }
        at += info.Size as usize;
    }
    sizes
}

/// Whether the cores of the largest last-level cache count as the fast
/// ones where all cores are of one class (`VICTORY_CACHE_CORES=0`: not).
/// On a processor with two core complexes of which one has the stacked cache
/// (Ryzen 7900X3D, 7950X3D, 9900X3D, 9950X3D) every core reports the same
/// efficiency class, so nothing was asked for: our threads ran on either
/// complex. Their work (compares, hashes, copies of tens of megabytes a
/// frame) is the kind the larger cache serves. Nothing changes on a processor
/// whose caches are all alike, or with two kinds of cores.
fn cache_cores() -> bool {
    #[cfg(feature = "diagnostics")]
    if let Some(on) = crate::ab::pick("VICTORY_CACHE_CORES") {
        return on;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_CACHE_CORES").is_none_or(|v| v != "0"))
}

/// The sets of the fastest class, if there are slower ones too.
fn fast_by_class(sets: &[(u32, u8, u8)]) -> Vec<u32> {
    let best = sets.iter().map(|c| c.1).max().unwrap_or(0);
    let fast: Vec<u32> = sets.iter().filter(|c| c.1 == best).map(|c| c.0).collect();
    if fast.len() == sets.len() { Vec::new() } else { fast }
}

/// The sets on the largest last-level cache (`l3`: bytes by set id), if
/// another one is smaller and the largest serves at least eight sets.
fn fast_by_cache(sets: &[(u32, u8, u8)], l3: &[(u32, u32)]) -> Vec<u32> {
    let size = |id: u32| l3.iter().find(|s| s.0 == id).map_or(0, |s| s.1);
    let sizes: Vec<u32> = sets.iter().map(|s| size(s.0)).collect();
    let (Some(&most), Some(&least)) = (sizes.iter().max(), sizes.iter().min()) else {
        return Vec::new();
    };
    if least == 0 || least == most {
        return Vec::new();
    }
    let fast: Vec<u32> = sets.iter().filter(|s| size(s.0) == most).map(|s| s.0).collect();
    if fast.len() < 8 { Vec::new() } else { fast }
}

/// The fast sets: those of the fastest class if there are slower ones, else
/// those of the largest cache ([`cache_cores`]). `now`: as things are at this
/// moment (else whether there could be any, for threads to ask at all).
fn fast_sets(sets: &[(u32, u8, u8)], now: bool) -> Vec<u32> {
    let by_class = fast_by_class(sets);
    if !by_class.is_empty() || (now && !cache_cores()) {
        return by_class;
    }
    static L3: std::sync::OnceLock<Vec<(u32, u32)>> = std::sync::OnceLock::new();
    fast_by_cache(sets, L3.get_or_init(l3_of_sets))
}

fn fast_of(sets: &[(u32, u8, u8)]) -> Vec<u32> {
    fast_sets(sets, true)
}

/// The sets of one fast core of the last, and all the others, for
/// [`lane_core`]; `None` with fewer than six fast cores.
fn one_core(sets: &[(u32, u8, u8)]) -> Option<(Vec<u32>, Vec<u32>)> {
    let best = sets.iter().map(|c| c.1).max()?;
    let mut cores: Vec<u8> = sets.iter().filter(|c| c.1 == best).map(|c| c.2).collect();
    cores.sort_unstable();
    cores.dedup();
    let core = *cores.last().filter(|_| cores.len() >= 6)?;
    let of = |own: bool| sets.iter().filter(|c| (c.2 == core) == own).map(|c| c.0).collect();
    Some((of(true), of(false)))
}

/// Whether the submitting thread has a fast core to itself, as far as this
/// process goes (`VICTORY_LANE_CORE=1`): nothing else of it runs on the
/// core's two threads. It was the one a frame waited for (94 % of it in a
/// street), and shared its core's units with whatever ran beside it.
fn lane_core() -> bool {
    #[cfg(feature = "diagnostics")]
    if let Some(on) = crate::ab::pick("VICTORY_LANE_CORE") {
        return on;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_LANE_CORE").is_some_and(|v| v == "1"))
}

/// Of the sets of the logical processors the title asked none of its threads
/// to run on (`asked`, a bit each): the fast ones (`Some(true)`), the slow
/// ones (`Some(false)`) or all. Empty if the title asked for nothing yet.
fn left_of(sets: &[(u32, u8, u8)], asked: u64, fast: Option<bool>) -> Vec<u32> {
    if asked == 0 {
        return Vec::new();
    }
    let best = sets.iter().map(|c| c.1).max().unwrap_or(0);
    sets.iter()
        .filter(|c| (c.2 >= 64 || asked >> c.2 & 1 == 0) && fast.is_none_or(|f| (c.1 == best) == f))
        .map(|c| c.0)
        .collect()
}

/// The logical processors of group 0 the title's threads were asked to run
/// on, all of its asks together ([`title_affinity`]).
static TITLE_ASKED: AtomicU64 = AtomicU64::new(0);

/// The title asked the system to run one of its threads on the logical
/// processors `mask` of group `group` (`victory-hle`'s
/// `SetThreadGroupAffinity`). It does so as on the console: its busiest
/// workers on the first eight, the rest on the six after them; on an
/// i9-12900K those are the first seven fast cores with both threads of each.
pub fn title_affinity(mask: usize, group: u16) {
    if group != 0 {
        return;
    }
    // Each mask once in the log, with how often it was asked so far.
    static SEEN: Mutex<Vec<(u64, u32)>> = Mutex::new(Vec::new());
    {
        let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
        match seen.iter_mut().find(|s| s.0 == mask as u64) {
            Some(s) => s.1 += 1,
            None => {
                seen.push((mask as u64, 1));
                tracing::info!(mask = format_args!("{mask:#x}"), "the title pins a thread to these processors, the first time");
            }
        }
    }
    // A mask of every processor pins nothing.
    let all = sets().0.iter().filter(|c| c.2 < 64).fold(0u64, |all, c| all | 1 << c.2);
    if mask as u64 & all == all {
        return;
    }
    let was = TITLE_ASKED.fetch_or(mask as u64, Relaxed);
    // Our threads that are kept off them move with what it asks.
    if was | mask as u64 != was && (apart() || lane_apart()) {
        place_again();
    }
}

/// The switches of where our threads run, each read from its variable (on
/// unless `0`) or changed while the title runs (`crate::ab`).
const SWITCHES: [&str; 2] = ["VICTORY_PLACE_APART", "VICTORY_PLACE_LANE"];

/// Each of [`SWITCHES`] as changed while the title runs: 0 as its variable
/// says, 1 on, 2 off.
static CHANGED: [AtomicU8; 2] = [const { AtomicU8::new(0) }; 2];

/// Whether switch `i` of [`SWITCHES`] is on.
fn switched(i: usize) -> bool {
    match CHANGED[i].load(Relaxed) {
        1 => return true,
        2 => return false,
        _ => {}
    }
    // Named in `VICTORY_AB`: off until its first phase on.
    #[cfg(feature = "diagnostics")]
    if let Some(on) = crate::ab::pick(SWITCHES[i]) {
        return on;
    }
    static ON: [OnceLock<bool>; 2] = [const { OnceLock::new() }; 2];
    *ON[i].get_or_init(|| std::env::var_os(SWITCHES[i]).is_none_or(|v| v != "0"))
}

/// Whether the threads that translate lists run on the slow cores the
/// title leaves (on by default; `VICTORY_PLACE_APART=0`: where they were).
///
/// Why: the title's threads run where it asks, as on the console. On an
/// i9-12900K its eight busy workers are on logical processors 0-7 (four
/// fast cores with both their threads), its main thread and ten others on
/// 8-12, one on 13. Ours were on the fast cores too, and the slow ones
/// stood idle. The eight threads that translate lists are the most work of
/// ours, and nothing waits for one of them alone.
///
/// Not all of ours: kept off every processor the title asks for (the line's
/// thread on 14-15, every other one on the slow cores), the frame was 5 ms
/// longer (split-pl3-apart). The helpers the line's thread and the sender
/// wait for (the heaps' records, refreshes spread over threads) were
/// slower there.
fn apart() -> bool {
    switched(0)
}

/// Whether the thread that prepares submissions runs on the fast cores the
/// title leaves (on by default; `VICTORY_PLACE_LANE=0`: on all fast ones):
/// 14-15 on an i9-12900K, where nothing of the title's runs.
fn lane_apart() -> bool {
    switched(1)
}

/// One of [`SWITCHES`] is `on` from now on (diagnostics, `crate::ab`): every
/// thread of ours is placed again, and what each kind of ours ran since the
/// last change is logged.
#[cfg(feature = "diagnostics")]
pub(crate) fn placing(switch: &str, on: bool) {
    let Some(i) = SWITCHES.iter().position(|s| *s == switch) else {
        return;
    };
    CHANGED[i].store(if on { 1 } else { 2 }, Relaxed);
    ran_line();
    place_again();
}

/// Diagnostics: the processor time of our threads that asked, by the name
/// they asked with, per second since the last line.
#[cfg(feature = "diagnostics")]
fn ran_line() {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::GetThreadTimes;
    static LAST: Mutex<Option<(std::time::Instant, std::collections::HashMap<usize, u64>)>> = Mutex::new(None);
    let ours = OURS.lock().unwrap_or_else(|e| e.into_inner());
    let mut now = std::collections::HashMap::new();
    for (handle, _, _) in ours.iter() {
        // SAFETY: zeroed FILETIMEs are valid; a thread of ours, opened with
        // the right to ask.
        let mut t: [FILETIME; 4] = unsafe { std::mem::zeroed() };
        let [a, b, c, d] = &mut t;
        unsafe { GetThreadTimes(*handle as *mut std::ffi::c_void, a, b, c, d) };
        let of = |f: &FILETIME| u64::from(f.dwHighDateTime) << 32 | u64::from(f.dwLowDateTime);
        now.insert(*handle, of(&t[2]) + of(&t[3]));
    }
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    // (Two switches changed at once: one line.)
    if last.as_ref().is_some_and(|(since, _)| since.elapsed() < std::time::Duration::from_secs(1)) {
        return;
    }
    if let Some((since, before)) = last.as_ref() {
        let seconds = since.elapsed().as_secs_f64().max(1e-3);
        let mut by: std::collections::BTreeMap<&str, (f64, u32)> = std::collections::BTreeMap::new();
        for (handle, _, what) in ours.iter() {
            let ran = now[handle].saturating_sub(before.get(handle).copied().unwrap_or(now[handle]));
            let e = by.entry(what.as_str()).or_default();
            *e = (e.0 + ran as f64 / 1e4 / seconds, e.1 + 1);
        }
        let text: Vec<String> = by.iter().map(|(what, (ms, n))| format!("{what} x{n} {ms:.0}")).collect();
        tracing::info!("our threads' processor time, ms a second, over {seconds:.1} s: {}", text.join(", "));
    }
    *last = Some((std::time::Instant::now(), now));
}

/// `VICTORY_FAST_CORES`: 0: no thread asks; 1: the threads a frame waits for
/// one by one; 2 (the default): the threads they share their work with too;
/// 3: those that work beside the rest as well (lists translated, descriptors
/// made): more threads by then than the fast cores run at a time, while the
/// slow ones stood idle.
fn stage() -> u32 {
    static STAGE: OnceLock<u32> = OnceLock::new();
    *STAGE.get_or_init(|| std::env::var("VICTORY_FAST_CORES").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(2))
}

/// Asks the system to run the calling thread, which a frame waits for, on
/// the fast cores only, where the processor has two kinds. That fork's submit
/// thread was found on a slow one in 1,529 looks of 24,021. (A selection of
/// processors is a preference the scheduler keeps unless nothing of it is
/// free.) The title's threads are not asked for.
pub(crate) fn keep_on_fast_cores(what: &str) {
    ask(what, 1);
}

/// [`keep_on_fast_cores`] for one of the threads a submission's preparing or
/// a landing shares its work with.
pub(crate) fn keep_helper_on_fast_cores(what: &str) {
    ask(what, 2);
}

/// [`keep_on_fast_cores`] for one of many threads whose work is ahead of what
/// a frame waits for.
pub(crate) fn keep_many_on_fast_cores(what: &str) {
    ask(what, 3);
}

/// The sets read once: by logical processor (id, class, logical
/// processor), for what the title leaves ([`left_of`]), and by core (id,
/// class, core), for the fast ones ([`fast_of`]) and one of them
/// ([`one_core`]).
fn sets() -> &'static (Vec<(u32, u8, u8)>, Vec<(u32, u8, u8)>) {
    static SETS: OnceLock<(Vec<(u32, u8, u8)>, Vec<(u32, u8, u8)>)> = OnceLock::new();
    SETS.get_or_init(|| {
        let records = cpu_set_records();
        let by = |f: fn(&(u32, u8, u8, u8)) -> u8| records.iter().map(|r| (r.0, r.1, f(r))).collect();
        (by(|r| r.3), by(|r| r.2))
    })
}

/// Where a thread that asked as `what` at stage `from` is to run, and what
/// that is called; `None`: wherever the system likes.
fn chosen(what: &str, from: u32) -> Option<(Vec<u32>, &'static str)> {
    let (sets, by_core) = sets();
    // (`VICTORY_CACHE_CORES` may change while the title runs.)
    let mut fast = fast_of(by_core);
    // `VICTORY_LANE_CORE=1`: the submitting thread alone on a fast core,
    // the others' fast cores without it.
    if let Some((own, _)) = one_core(by_core).filter(|_| lane_core()) {
        if what == "victory-submit" {
            return Some((own, "a fast core of its own (VICTORY_LANE_CORE)"));
        }
        fast.retain(|id| !own.contains(id));
    }
    let asked = TITLE_ASKED.load(Relaxed);
    let kept = match what {
        "victory-list" if apart() => Some((false, "the slow cores the title leaves (VICTORY_PLACE_APART)")),
        "victory-submit" if lane_apart() => Some((true, "the fast cores the title leaves (VICTORY_PLACE_LANE)")),
        _ => None,
    };
    if let Some((fast_ones, kind)) = kept
        && asked != 0
    {
        let left = left_of(sets, asked, Some(fast_ones));
        if !left.is_empty() {
            return Some((left, kind));
        }
        // The title leaves none of that kind: where it would have been.
        static SAID: [std::sync::Once; 2] = [const { std::sync::Once::new() }; 2];
        SAID[usize::from(fast_ones)].call_once(|| {
            let cores = if fast_ones { "fast" } else { "slow" };
            tracing::info!(what, title = format_args!("{asked:#x}"), "the title leaves no {cores} cores: the thread stays where it was ({kind})");
        });
    }
    (stage() >= from && !fast.is_empty()).then_some((fast, "the fast cores (VICTORY_FAST_CORES=0: not)"))
}

/// Our threads that asked: a handle each, the stage and the name they asked
/// with.
static OURS: Mutex<Vec<(usize, u32, String)>> = Mutex::new(Vec::new());

fn ask(what: &str, from: u32) {
    use windows_sys::Win32::System::Threading::{
        GetCurrentThreadId, OpenThread, THREAD_QUERY_LIMITED_INFORMATION, THREAD_SET_LIMITED_INFORMATION,
    };
    // SAFETY: this thread of our own process; the handle is kept for good
    // (our threads that ask live as long as the process).
    let handle = unsafe { OpenThread(THREAD_SET_LIMITED_INFORMATION | THREAD_QUERY_LIMITED_INFORMATION, 0, GetCurrentThreadId()) };
    if handle.is_null() {
        return;
    }
    if lane_core() {
        process_default();
    }
    let mut ours = OURS.lock().unwrap_or_else(|e| e.into_inner());
    ours.push((handle as usize, from, what.to_string()));
    // (Under the lock: a placing again does not come between. Nothing to
    // choose: as before, nothing asked.)
    if let Some((sets, kind)) = chosen(what, from) {
        select(handle as usize, what, &sets, kind, true);
    }
}

/// Every thread of the process is kept off the submitting thread's own core
/// ([`lane_core`]), or may run anywhere.
fn process_default() {
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, SetProcessDefaultCpuSets};
    if stage() == 0 {
        return;
    }
    let one = one_core(&sets().1).filter(|_| lane_core());
    let others: &[u32] = one.as_ref().map_or(&[], |o| &o.1);
    let at = if others.is_empty() { std::ptr::null() } else { others.as_ptr() };
    // SAFETY: our own process and the system's own set ids (none: all).
    unsafe { SetProcessDefaultCpuSets(GetCurrentProcess(), at, others.len() as u32) };
}

/// What the threads of the process, and those of ours that asked, are told
/// now ([`lane_core`] and [`cache_cores`], which may have changed while the
/// title runs).
#[cfg(feature = "diagnostics")]
pub(crate) fn settle() {
    if stage() == 0 {
        return;
    }
    process_default();
    place_again();
    let by_core = &sets().1;
    let one = one_core(by_core).filter(|_| lane_core());
    tracing::info!(on = one.is_some(), core = ?one.as_ref().map(|o| &o.0), "the submitting thread has a core of its own (VICTORY_LANE_CORE)");
    let fast = fast_of(by_core);
    tracing::info!(
        fast = fast.len(),
        of = by_core.len(),
        by_cache = fast_by_class(by_core).is_empty() && !fast.is_empty(),
        "our threads' processors (VICTORY_CACHE_CORES)"
    );
}

/// Asks for `sets` (none: the system's choice) for the thread `handle`.
fn select(handle: usize, what: &str, sets: &[u32], kind: &str, say: bool) {
    use windows_sys::Win32::System::Threading::SetThreadSelectedCpuSets;
    let (ids, count) = if sets.is_empty() { (std::ptr::null(), 0) } else { (sets.as_ptr(), sets.len() as u32) };
    // SAFETY: a thread of ours, opened for this; a list of the system's own
    // set ids, or none.
    let ok = unsafe { SetThreadSelectedCpuSets(handle as *mut std::ffi::c_void, ids, count) };
    if ok == 0 && say {
        tracing::warn!(what, error = %std::io::Error::last_os_error(), "the thread could not be kept on {kind}");
    } else if ok != 0 && say {
        let title = TITLE_ASKED.load(Relaxed);
        tracing::info!(what, sets = sets.len(), of = self::sets().0.len(), title = format_args!("{title:#x}"), "the thread is kept on {kind}");
    }
}

/// Every thread of ours that asked is placed again, as [`chosen`] says now.
fn place_again() {
    let ours = OURS.lock().unwrap_or_else(|e| e.into_inner());
    for (handle, from, what) in ours.iter() {
        match chosen(what, *from) {
            Some((sets, kind)) => select(*handle, what, &sets, kind, false),
            None => select(*handle, what, &[], "the processors the system chooses", false),
        }
    }
    tracing::info!(
        threads = ours.len(),
        apart = apart(),
        lane = lane_apart(),
        title = format_args!("{:#x}", TITLE_ASKED.load(Relaxed)),
        "our threads are placed again (VICTORY_PLACE_*)"
    );
}

/// Whether the threads the submitting thread waits for run at its priority
/// (`VICTORY_HELPER_PRIORITY=1`; off by default; `VICTORY_AB` can flip it
/// while the title runs): above normal, ahead of the title's threads and of
/// the threads that translate lists. They are the threads that spread its work
/// (`shadow::wide`, the pools of `land`), the one that makes the heaps'
/// records and the one that passes over the heap's views beside the lists.
/// With `VICTORY_WIDE_SPIN` set, the `shadow::wide` workers busy-wait at that
/// priority between jobs.
///
/// The submitting thread is above normal (`queue::behind`); its helpers are
/// not, so on a busy processor it can stand waiting for helpers that are not
/// running. Measured on a six-core processor (i5-12400F, 12 threads, RTX 3060,
/// `-Machine 4070`, Series S at half scale) with `VICTORY_AB`, standing still
/// in one scene with the same draws in every phase (off / on): 11.6 / 11.7
/// fps, refreshes 19.1 / 16.9 ms a frame, waiting for the heaps' records 0.5
/// / 0.0 ms, the title's threads waiting at presents 33.9 / 28.9 ms. The
/// waits shrink, the frame rate did not move, and the helpers then run ahead
/// of the title's own normal-priority threads (simulation, audio, streaming),
/// which on a smaller or busier processor may cost more than it gives: off
/// until another processor measures it.
fn helpers_raised() -> bool {
    #[cfg(feature = "diagnostics")]
    if let Some(on) = crate::ab::pick("VICTORY_HELPER_PRIORITY") {
        return on;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| crate::switch::on("VICTORY_HELPER_PRIORITY"))
}

/// A helper's priority ([`helpers_raised`]). [`Self::keep`] is called before
/// each piece of work; it asks the system only when the switch has changed.
pub(crate) struct Priority {
    requested: Option<bool>,
}

impl Priority {
    pub(crate) fn new() -> Self {
        let mut priority = Self { requested: None };
        priority.keep();
        priority
    }

    pub(crate) fn keep(&mut self) {
        use windows_sys::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL, THREAD_PRIORITY_NORMAL,
        };
        let on = helpers_raised();
        if self.requested == Some(on) {
            return;
        }
        let priority = if on { THREAD_PRIORITY_ABOVE_NORMAL } else { THREAD_PRIORITY_NORMAL };
        // SAFETY: our own thread.
        if unsafe { SetThreadPriority(GetCurrentThread(), priority) } == 0 {
            let error = std::io::Error::last_os_error();
            static SAID: std::sync::Once = std::sync::Once::new();
            SAID.call_once(|| {
                tracing::warn!(%error, priority, "could not set the submitting helper's priority");
            });
        }
        self.requested = Some(on);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fast_cores_are_those_of_the_best_class_if_there_is_a_worse_one() {
        assert_eq!(fast_by_class(&[(256, 1, 0), (257, 1, 0), (258, 0, 2)]), [256, 257]);
        assert!(fast_by_class(&[(256, 0, 0), (257, 0, 1)]).is_empty());
        assert!(one_core(&[(256, 1, 0), (257, 1, 0), (258, 0, 2)]).is_none());
        let eight: Vec<(u32, u8, u8)> = (0..16).map(|n| (n, 1, n as u8 / 2 * 2)).chain((16..24).map(|n| (n, 0, n as u8))).collect();
        let (own, others) = one_core(&eight).unwrap();
        assert_eq!((own, others.len()), (vec![14, 15], 22));
        assert!(fast_by_class(&[]).is_empty());
        assert!(!cpu_sets().is_empty());
        std::thread::spawn(|| keep_on_fast_cores("test")).join().unwrap();
    }

    const MB: u32 = 1 << 20;

    /// `n` sets of one class, two to a core, from id 256.
    fn alike(n: u32) -> Vec<(u32, u8, u8)> {
        (0..n).map(|i| (256 + i, 0, (i / 2) as u8)).collect()
    }

    #[test]
    fn the_largest_cache_is_fast_only_where_the_caches_differ() {
        // Two complexes, one with the stacked cache (9950X3D: 16 sets each).
        let sets = alike(32);
        let l3: Vec<(u32, u32)> = (0..32).map(|i| (256 + i, if i < 16 { 96 * MB } else { 32 * MB })).collect();
        assert_eq!(fast_by_cache(&sets, &l3), (256..272).collect::<Vec<_>>());
        // The stacked cache on the second complex: that one.
        let flipped: Vec<(u32, u32)> = (0..32).map(|i| (256 + i, if i < 16 { 32 * MB } else { 96 * MB })).collect();
        assert_eq!(fast_by_cache(&sets, &flipped), (272..288).collect::<Vec<_>>());
        // Two complexes alike (9950X): nothing.
        let even: Vec<(u32, u32)> = (0..32).map(|i| (256 + i, 32 * MB)).collect();
        assert!(fast_by_cache(&sets, &even).is_empty());
        // One complex (7800X3D, 9800X3D): nothing.
        let one: Vec<(u32, u32)> = (0..16).map(|i| (256 + i, 96 * MB)).collect();
        assert!(fast_by_cache(&alike(16), &one).is_empty());
        // Caches not known: nothing.
        assert!(fast_by_cache(&sets, &[]).is_empty());
        assert!(fast_by_cache(&sets, &l3[..16]).is_empty());
        // A larger cache of under eight sets: nothing.
        let few: Vec<(u32, u32)> = (0..32).map(|i| (256 + i, if i < 4 { 64 * MB } else { 32 * MB })).collect();
        assert!(fast_by_cache(&sets, &few).is_empty());
    }

    #[test]
    fn two_kinds_of_cores_still_go_by_their_class() {
        // Eight fast cores (two sets each) and eight slow ones, the caches
        // told to differ too: the class decides, whatever the caches say.
        let sets: Vec<(u32, u8, u8)> =
            (0..16).map(|n| (n, 1, n as u8 / 2 * 2)).chain((16..24).map(|n| (n, 0, n as u8))).collect();
        assert_eq!(fast_sets(&sets, true), (0..16).collect::<Vec<_>>());
        assert_eq!(fast_sets(&sets, false), (0..16).collect::<Vec<_>>());
    }

    /// The title on the first two fast processors of three: of the rest,
    /// the fast one, the slow one, or both.
    #[test]
    fn what_the_title_leaves_is_found_by_kind() {
        let four = [(256, 1, 0), (257, 1, 1), (258, 1, 2), (259, 0, 3)];
        assert_eq!(left_of(&four, 0b0011, Some(true)), [258]);
        assert_eq!(left_of(&four, 0b0011, Some(false)), [259]);
        assert_eq!(left_of(&four, 0b0011, None), [258, 259]);
        assert!(left_of(&four, 0, None).is_empty(), "the title asked for nothing yet");
        assert!(left_of(&four, 0b1111, None).is_empty(), "it left nothing");
        assert!(left_of(&four, 0b0111, Some(true)).is_empty(), "no fast one left");
    }
}
