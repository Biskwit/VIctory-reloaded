//! Host pipelines are made by workers, not by the title thread that first
//! records a draw with them. Translating a shader and the host driver's
//! compile take from milliseconds to seconds each (structurisation of the
//! biggest compute programs ~0.4 s, some driver compiles 3.4 s), and Grand
//! Theft Auto VI's first world frame wants over 1,200 new pipelines: made
//! on its recording threads, that frame took more than 30 s and the
//! title's GPU watchdog ended the run (decision 0007, "Pipelines made off
//! the title's threads").
//!
//! A first use queues the pipeline and waits for it within a budget per
//! frame, so the cheap ones (menus, one-off passes such as a colour-grading
//! bake) are there at once as before; past the budget a draw or dispatch
//! whose pipeline is not ready is dropped, as an untranslated one is.
//!
//! shortcut: the budget (half a second of waiting per title frame) is a
//! guess; a pass the title runs only once and whose pipeline takes longer
//! is lost.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::Shader;
use super::host::{GraphicsState, HostPipeline};

/// What a host pipeline is made from, and the host pipeline once made.
pub(crate) struct Pending {
    shaders: Vec<Arc<Shader>>,
    pub(crate) graphics: Option<Box<GraphicsState>>,
    queue_state: AtomicU8,
    made: OnceLock<Option<HostPipeline>>,
    /// What identifies it across runs (shader containers and state), and
    /// whether a draw has used it yet.
    key: String,
    used: AtomicBool,
}

// SAFETY: D3D12 objects are free-threaded; the graphics state (whose input
// elements point into its own names) is only read after creation.
unsafe impl Send for Pending {}
// SAFETY: as above.
unsafe impl Sync for Pending {}

impl Pending {
    /// Its shaders' hashes, as logged (diagnostics).
    pub(crate) fn hashes(&self) -> Vec<&str> {
        self.shaders.iter().map(|s| s.hash.as_str()).collect()
    }

    pub(crate) fn new(shaders: Vec<Arc<Shader>>, graphics: Option<Box<GraphicsState>>) -> Pending {
        let mut key = blake3::Hasher::new();
        for s in &shaders {
            key.update(s.stage.as_bytes());
            key.update(s.hash.as_bytes());
        }
        key.update(graphics.as_ref().map(|g| g.describe()).unwrap_or_default().as_bytes());
        Pending {
            shaders,
            graphics,
            queue_state: AtomicU8::new(NOT_QUEUED),
            made: OnceLock::new(),
            key: key.finalize().to_hex()[..32].to_owned(),
            used: AtomicBool::new(false),
        }
    }

    fn make(&self) {
        self.made
            .get_or_init(|| HostPipeline::new(&self.shaders, self.graphics.as_deref()));
        let (lock, done) = &DONE;
        let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
        done.notify_all();
    }
}

/// Why a pipeline has no host pipeline (diagnostics): its shaders that do
/// not translate, else "refused" or "being made"; `None` if it has one.
#[cfg(feature = "diagnostics")]
pub(crate) fn missing(p: &Pending) -> Option<String> {
    match p.made.get() {
        Some(Some(_)) => None,
        None => Some("being made".into()),
        Some(None) => {
            let failed: Vec<String> = p
                .shaders
                .iter()
                .filter(|s| matches!(s.translated.get(), Some(None)))
                .map(|s| format!("{} {}", s.stage, s.hash))
                .collect();
            Some(if failed.is_empty() {
                format!("refused {:?}", p.shaders)
            } else {
                failed.join(" + ")
            })
        }
    }
}

/// Signalled whenever a worker has made a pipeline.
static DONE: (Mutex<()>, Condvar) = (Mutex::new(()), Condvar::new());

/// Waiting allowed per title frame, over all threads.
const BUDGET: Duration = Duration::from_millis(500);

/// Pipelines queued and not made yet.
static WAITING: AtomicU64 = AtomicU64::new(0);

/// Pipelines queued and not made yet (for the frame line).
pub(crate) fn waiting() -> u64 {
    WAITING.load(Ordering::Relaxed)
}

/// The host pipeline if it is made (or is made within the frame's budget);
/// queues it on first use.
pub(crate) fn ready(p: &Arc<Pending>) -> Option<&HostPipeline> {
    if p.made.get().is_none() {
        let priority = if urgent_compute() && is_compute(p) { URGENT } else { REQUESTED };
        start(p, priority);
        let frame = crate::frame::current();
        if !(is_compute(p) && wait_compute(p, frame)) {
            wait(p, frame);
        }
    }
    let made = p.made.get()?.as_ref();
    // Also one that could not be made (a shader that does not translate):
    // the next run then finds that out while loading, like the others,
    // instead of in the world's frames, where each first use of such a
    // pipeline waits on the frame's budget for nothing.
    if !p.used.swap(true, Ordering::Relaxed) {
        super::library::note_used(&p.key);
    }
    made
}

/// Makes `p` on this thread if it is not made yet (waiting for a worker
/// that is making it); whether it has a host pipeline.
pub(crate) fn now(p: &Arc<Pending>) -> bool {
    p.make();
    p.made.get().is_some_and(|made| made.is_some())
}

/// At the title's creation of a pipeline: starts it at once if an earlier
/// run drew with it (during loading, so the world's first frames find it).
pub(crate) fn early(p: &Arc<Pending>) {
    if !p.shaders.is_empty() && super::library::wanted_early(&p.key) {
        super::library::EARLY.fetch_add(1, Ordering::Relaxed);
        start(p, EARLY);
    }
}

/// Whether the frame's budget is one for all threads together: a thread
/// that waits takes what is left of it and gives back what it did not
/// use. Counted only when a wait had ended, the budget was one per thread:
/// each of the title's five recording threads began its first wait of a
/// frame with nothing counted yet and waited the whole half second, 2.5 s
/// of waiting in every frame in which pipelines were being made (runs
/// 34-49: "2504 ms waiting for pipelines" in each such frame).
/// On by default; `VICTORY_PIPELINE_WAIT_SHARED=0` counts as before.
fn shared_budget() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_PIPELINE_WAIT_SHARED").is_none_or(|v| v != "0"))
}

/// Waiting allowed for one compute pipeline and for all of a title
/// frame's: `VICTORY_COMPUTE_WAIT=<per pipeline ms>,<per frame ms>`
/// (default 5000,15000; 0: no such wait, the frame's budget as for draws).
fn compute_wait() -> (Duration, Duration) {
    static WAIT: OnceLock<(Duration, Duration)> = OnceLock::new();
    *WAIT.get_or_init(|| {
        let value = std::env::var("VICTORY_COMPUTE_WAIT").unwrap_or_default();
        let mut parts = value.split(',').map(|v| v.trim().parse::<u64>().ok());
        let per = parts.next().flatten().unwrap_or(5000);
        let frame = parts.next().flatten().unwrap_or(15_000);
        (Duration::from_millis(per), Duration::from_millis(frame))
    })
}

/// A dispatch waits for its compute pipeline being made (first in the
/// queue, [`urgent_compute`]) up to [`compute_wait`]'s time for it and
/// what is left of the frame's, instead of being dropped after the frame's
/// half-second budget for draws: a compute pass often keeps state from
/// frame to frame, which a launch that drops it for minutes never gets back
/// (the pale picture of cold launches, `urgent_compute`). Capped, because
/// a wait for every pipeline the title asks for ended the title (72 s into
/// a cold launch's first world frame). False if there is no such wait
/// left (the caller waits on the frame's budget).
fn wait_compute(p: &Pending, frame: u64) -> bool {
    /// The frame the waits belong to, waited so far, pipelines waited for,
    /// and of them those not made in time.
    static SPENT: Mutex<(u64, Duration, u32, u32)> = Mutex::new((u64::MAX, Duration::ZERO, 0, 0));
    let (per, cap) = compute_wait();
    let left = {
        let mut spent = SPENT.lock().unwrap_or_else(|e| e.into_inner());
        if spent.0 != frame {
            // The frame before, if it waited long: for the caps' measure.
            if spent.1 >= Duration::from_secs(1) {
                tracing::info!(
                    frame = spent.0,
                    ms = spent.1.as_millis() as u64,
                    pipelines = spent.2,
                    not_in_time = spent.3,
                    "dispatches waited for their compute pipelines"
                );
            }
            *spent = (frame, Duration::ZERO, 0, 0);
        }
        per.min(cap.saturating_sub(spent.1))
    };
    if left.is_zero() {
        return false;
    }
    let started = Instant::now();
    let (lock, done) = &DONE;
    let mut held = lock.lock().unwrap_or_else(|e| e.into_inner());
    while p.made.get().is_none() {
        let rest = left.saturating_sub(started.elapsed());
        if rest.is_zero() {
            break;
        }
        held = done.wait_timeout(held, rest).unwrap_or_else(|e| e.into_inner()).0;
    }
    drop(held);
    let mut spent = SPENT.lock().unwrap_or_else(|e| e.into_inner());
    if spent.0 == frame {
        spent.1 += started.elapsed();
        spent.2 += 1;
        spent.3 += u32::from(p.made.get().is_none());
    }
    true
}

/// Waits for `p` while frame `frame` has budget left.
fn wait(p: &Pending, frame: u64) {
    // The frame the budget belongs to, and what was waited of it (or is
    // taken by a thread waiting now).
    static SPENT: Mutex<(u64, Duration)> = Mutex::new((u64::MAX, Duration::ZERO));
    let shared = shared_budget();
    let left = {
        let mut spent = SPENT.lock().unwrap_or_else(|e| e.into_inner());
        if spent.0 != frame {
            *spent = (frame, Duration::ZERO);
        }
        let left = BUDGET.saturating_sub(spent.1);
        if shared {
            spent.1 += left;
        }
        left
    };
    if left.is_zero() {
        return;
    }
    let started = Instant::now();
    let (lock, done) = &DONE;
    let mut held = lock.lock().unwrap_or_else(|e| e.into_inner());
    while p.made.get().is_none() {
        let rest = left.saturating_sub(started.elapsed());
        if rest.is_zero() {
            break;
        }
        held = done.wait_timeout(held, rest).unwrap_or_else(|e| e.into_inner()).0;
    }
    drop(held);
    let waited = started.elapsed().min(left);
    {
        let mut spent = SPENT.lock().unwrap_or_else(|e| e.into_inner());
        // A frame that began meanwhile has its own budget.
        if spent.0 == frame {
            if shared {
                spent.1 = spent.1.saturating_sub(left - waited);
            } else {
                spent.1 += waited;
            }
        }
    }
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(
        crate::copystat::C::PipelineWaitUs,
        started.elapsed().as_micros() as u64,
    );
}

const NOT_QUEUED: u8 = 0;
const EARLY: u8 = 1;
const REQUESTED: u8 = 2;
/// A compute pipeline a dispatch asked for ([`urgent_compute`]).
const URGENT: u8 = 3;
const MAKING: u8 = 4;

/// Whether a compute pipeline a dispatch asks for goes ahead of every other
/// queued pipeline, not in turn behind the draws' (`VICTORY_COMPUTE_FIRST=0`:
/// in turn, as before). A cold launch's first world frame asks for some
/// 2,000 pipelines at once (1,932 first uses within 5 s, run chain-cold),
/// made in the order asked: the compute chain that keeps a persistent
/// 384x384 lighting map (ef05293f, 4ceef06f, f42a5dca, 5b2ce592, 89106710)
/// waited behind 1,285 others, four minutes, while the passes reading its
/// map ran; the map then started from that stale state and the launch kept
/// the pale picture. Compute passes keep state from frame to frame like
/// that; a draw dropped for a frame leaves nothing behind.
fn urgent_compute() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_COMPUTE_FIRST").is_none_or(|v| v != "0"))
}

/// Whether `p` is a compute pipeline (one compute shader).
fn is_compute(p: &Pending) -> bool {
    matches!(&p.shaders[..], [s] if s.stage == "compute")
}

struct Queue {
    urgent: VecDeque<Arc<Pending>>,
    early: VecDeque<Arc<Pending>>,
    requested: VecDeque<Arc<Pending>>,
}

impl Queue {
    const fn new() -> Self {
        Self {
            urgent: VecDeque::new(),
            early: VecDeque::new(),
            requested: VecDeque::new(),
        }
    }

    /// Queues an unrequested pipeline or promotes one queued at creation.
    /// Returns whether it is new to the queue, or `None` if nothing changed.
    fn push(&mut self, p: &Arc<Pending>, priority: u8) -> Option<bool> {
        let prev = p.queue_state.load(Ordering::Relaxed);
        if prev >= priority {
            return None;
        }
        p.queue_state.store(priority, Ordering::Relaxed);
        match priority {
            URGENT => self.urgent.push_back(p.clone()),
            REQUESTED => self.requested.push_back(p.clone()),
            _ => self.early.push_back(p.clone()),
        }
        Some(prev == NOT_QUEUED)
    }

    fn pop(&mut self) -> Option<Arc<Pending>> {
        // An entry can have been promoted (and claimed from the queue it
        // was promoted to) since it was queued: only taken at its state.
        let at = |q: &mut VecDeque<Arc<Pending>>, state: u8| {
            std::iter::from_fn(|| q.pop_front()).find(|p| p.queue_state.load(Ordering::Relaxed) == state)
        };
        let p = at(&mut self.urgent, URGENT)
            .or_else(|| at(&mut self.requested, REQUESTED))
            .or_else(|| at(&mut self.early, EARLY))?;
        p.queue_state.store(MAKING, Ordering::Relaxed);
        Some(p)
    }
}

static QUEUE: Mutex<Queue> = Mutex::new(Queue::new());
static AVAILABLE: Condvar = Condvar::new();

/// Start or promote work for a draw without waiting behind the entire early
/// list. Queue changes and worker claims happen under the same lock.
fn start(p: &Arc<Pending>, priority: u8) {
    if p.queue_state.load(Ordering::Relaxed) >= priority {
        return;
    }
    if !workers() {
        if p.queue_state.swap(MAKING, Ordering::Relaxed) == NOT_QUEUED {
            p.make();
        }
        return;
    }
    let mut queue = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(new) = queue.push(p, priority) {
        if new {
            PEAK.fetch_max(WAITING.fetch_add(1, Ordering::Relaxed) + 1, Ordering::Relaxed);
        }
        AVAILABLE.notify_one();
    }
}

fn next() -> Arc<Pending> {
    let mut queue = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        if let Some(p) = queue.pop() {
            return p;
        }
        queue = AVAILABLE.wait(queue).unwrap_or_else(|e| e.into_inner());
    }
}

/// Starts the workers (half the logical processors, 2 to 8) on first use.
fn workers() -> bool {
    static RUNNING: OnceLock<bool> = OnceLock::new();
    *RUNNING.get_or_init(|| {
        let mut running = false;
        let workers = std::thread::available_parallelism().map_or(2, |n| (n.get() / 2).clamp(2, 8));
        for i in 0..workers {
            let spawned = std::thread::Builder::new()
                .name(format!("victory-pipelines-{i}"))
                .spawn(|| {
                    // Below the title's own threads (loading, recording).
                    // SAFETY: a plain call on our own thread.
                    unsafe {
                        use windows::Win32::System::Threading::*;
                        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
                    }
                    loop {
                        next().make();
                        if WAITING.fetch_sub(1, Ordering::Relaxed) == 1 {
                            drained();
                        } else {
                            super::library::save(false);
                        }
                    }
                });
            if let Err(e) = spawned {
                tracing::warn!(error = %e, "no pipeline worker");
            } else {
                running = true;
            }
        }
        running
    })
}

/// The most pipelines waiting at once since the workers last ran out.
static PEAK: AtomicU64 = AtomicU64::new(0);

/// The workers ran out of work: logged after a batch of 10 or more, and
/// the cache saved (at most every 20 s).
fn drained() {
    use super::library::{EARLY, LOADED, MADE};
    if PEAK.swap(0, Ordering::Relaxed) >= 10 {
        tracing::info!(
            made = MADE.load(Ordering::Relaxed),
            loaded = LOADED.load(Ordering::Relaxed),
            started_early = EARLY.load(Ordering::Relaxed),
            "host pipelines all made"
        );
    }
    super::library::save(false);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A first use queues the pipeline and, within the budget, gets the
    /// worker's answer (here: no host pipeline for no shaders).
    #[test]
    fn first_use_is_made_by_a_worker() {
        let p = Arc::new(Pending::new(Vec::new(), None));
        assert!(ready(&p).is_none());
        assert_ne!(p.queue_state.load(Ordering::Relaxed), NOT_QUEUED);
        let started = Instant::now();
        while p.made.get().is_none() && started.elapsed() < Duration::from_secs(10) {
            std::thread::yield_now();
        }
        assert!(matches!(p.made.get(), Some(None)));
    }

    #[test]
    fn draws_promote_early_pipelines_without_claiming_them_twice() {
        let earlier = Arc::new(Pending::new(Vec::new(), None));
        let promoted = Arc::new(Pending::new(Vec::new(), None));
        let requested = Arc::new(Pending::new(Vec::new(), None));
        let mut queue = Queue::new();

        assert_eq!(queue.push(&earlier, EARLY), Some(true));
        assert_eq!(queue.push(&promoted, EARLY), Some(true));
        assert_eq!(queue.push(&requested, REQUESTED), Some(true));
        assert_eq!(queue.push(&promoted, REQUESTED), Some(false));
        assert_eq!(queue.push(&promoted, REQUESTED), None);

        assert!(Arc::ptr_eq(&queue.pop().unwrap(), &requested));
        assert!(Arc::ptr_eq(&queue.pop().unwrap(), &promoted));
        assert_eq!(queue.push(&promoted, REQUESTED), None);
        assert!(Arc::ptr_eq(&queue.pop().unwrap(), &earlier));
        assert!(queue.pop().is_none());
    }

    /// A compute pipeline a dispatch asks for is made before every draw's
    /// and early pipeline queued before it, once (also when it was queued
    /// as a draw's first).
    #[test]
    fn urgent_pipelines_go_first_and_once() {
        let early = Arc::new(Pending::new(Vec::new(), None));
        let drawn = Arc::new(Pending::new(Vec::new(), None));
        let chain = Arc::new(Pending::new(Vec::new(), None));
        let mut queue = Queue::new();

        assert_eq!(queue.push(&early, EARLY), Some(true));
        assert_eq!(queue.push(&drawn, REQUESTED), Some(true));
        assert_eq!(queue.push(&chain, REQUESTED), Some(true));
        // The dispatch asks: promoted ahead of the draw queued before it.
        assert_eq!(queue.push(&chain, URGENT), Some(false));

        assert!(Arc::ptr_eq(&queue.pop().unwrap(), &chain));
        assert!(Arc::ptr_eq(&queue.pop().unwrap(), &drawn));
        assert!(Arc::ptr_eq(&queue.pop().unwrap(), &early));
        assert!(queue.pop().is_none());
    }
}
