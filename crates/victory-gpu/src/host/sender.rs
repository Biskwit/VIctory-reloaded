//! The thread that sends prepared submissions (`VICTORY_SUBMIT_SPLIT`), and
//! what is left to it: moved out of `host.rs` as it is (the file
//! policy's size limit), with `use super::*` for the rest of the host.

use super::*;

/// Whether more of a published submission is left to others than the line's
/// thread (on by default with [`split_send`]; `VICTORY_OFF_LINE=0`: as
/// before; switched by `VICTORY_AB` too): the slots its steps on the card
/// fill ([`card_steps::Slots`]) are written by [`sender`], and host lists
/// are closed when they are reset after running (`crate::list::recycle`),
/// not when one is taken for a title's list (`crate::list::detach`).
///
/// Why: the line's thread was 37 of a 39 ms frame (split-now-tl-3): 1.9 ms
/// of it the slots (11,000 a frame, 7 ms on all threads), 1.1 ms closing 127
/// new host lists a frame, each empty. The sender was idle 30 ms. Frames
/// 1.7 ms shorter (split-now-off2-1..3).
///
/// The slots, published: the sender writes them after the submission's
/// heaps' records and before its lists go to the queue, in the order
/// submissions were published, as the slots that wait for textures.
/// - Nothing of the preparing reads host heap slots (what a draw reads
///   through a table it made itself, when recorded), so the next
///   submission's preparing does not wait for them.
/// - Records the title copied before the submission are made first, and
///   the slot after them, as the card's fill comes after them on the
///   console. (Before, the line's thread wrote the slot first and the
///   records' round after it.) In the cell, no slot such a step fills had
///   its title record changed at all (`VICTORY_SLOT_ORDER_COUNT=1`,
///   split-now-cnt-1): the order changed nothing there.
/// - The title writes such a slot again only once the submission ran (its
///   fence), so no later record of its is written over.
/// - A slot two submissions fill is written in their order: both by the
///   sender, or the later one by the line's thread once every published
///   one was sent ([`wait_sent`]).
///
/// The lists: a list taken for a title's list is closed (its next call is a
/// reset). Reset and closed when it is given back, it is taken closed; one
/// taken open (new, or given back with this off) is left open for that
/// reset to close ([`HostList::left_open`], Vice-Tay 104c3d36), and one
/// taken closed to record into is reset again.
pub(crate) fn off_line() -> bool {
    #[cfg(feature = "diagnostics")]
    if let Some(on) = crate::ab::pick("VICTORY_OFF_LINE") {
        return on && split_send();
    }
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_OFF_LINE").is_none_or(|v| v != "0")) && split_send()
}

/// Whether a submission's preparing and its sending are done by two threads
/// in a row (on by default; `VICTORY_SUBMIT_SPLIT=0`: one thread does both,
/// as before): the line's thread prepares the next submission while
/// [`sender`] sends this one. Only with the relay's second stage, its
/// fetches on the copy queue and the sends taken at preparing
/// (`relay_exact`, `relay_copy`, `send::own`).
///
/// One thread did both, 45 of a 47 ms frame (timeline split-tl-1), some 7 ms
/// of it after the submission was prepared: the lists that send to copies
/// in the card's memory and the compare, the wait for the heaps' records,
/// the slots that wait for textures, the lists' sending. Nothing of the next
/// submission's preparing needs those.
///
/// The line's thread prepares as before, takes what goes up to copies in the
/// card's memory (`card_take`, numbered in the queue's order) and puts the
/// placed copies' bytes into their references. Then it *publishes* the
/// submission under the submission lock: its fence value, and its flight,
/// bottom-of-pipe writes and declared ranges in [`LINE`]; the rest goes to
/// [`SENDS`]. The sender takes them in that order, records what was taken
/// (`card_record`), gives the flight its compare plan, waits for the heaps'
/// records, sends the lists and signals the fence value.
///
/// What the next submission's preparing may assume of a published one:
/// - It is in the air. Whatever asks what flights write (the line's ranges,
///   the copies done on the card, bottom-of-pipe writes, `writes_pending`)
///   sees it, so the next one waits for it to land where it would have, or
///   has the card read behind it. Such a wait ends: the relay lands it once
///   the card reached its fence, and the sender needs nothing the waiter
///   holds (below).
/// - Its CPU work is done: title memory its steps wrote, its declared ranges
///   and snapshots, its bytes for copies in the card's memory (taken, so the
///   next one's refreshes of those CPU sides do not reach it), its placed
///   copies' references.
/// - Not done: nothing of it is on the queue, its compare plan is not made
///   (only [`Host::relay_land`] reads it, once the card ran it), its slots
///   that wait for textures are not made, nor (with [`off_line`]) the slots
///   its steps on the card fill.
///
/// Order: no submission of ours goes to the queue ahead of a published one.
/// Every other one (waited for, the present's copy) first waits, under the
/// submission lock, until every one published was sent ([`wait_sent`]), and
/// gets a higher fence value. The relay's fetches go to the copy queue and
/// know their order from their plans' numbers, not from the drawing queue.
///
/// Locks: the sender never takes the submission lock nor waits for the
/// line's thread, only for landings of flights it sent (`card_results_low`)
/// and for the heaps' records' thread. So a holder of the submission lock
/// may wait for published flights to land (a title's thread at a blocking
/// fence wait, a submission that is waited for, the line's thread).
///
/// A published submission is sent or the host fails: the sender alone takes
/// from [`SENDS`] and does every one; a failed send or signal, a lost card or
/// a panic on it is [`fail`], as on the one thread before.
pub(super) fn split_send() -> bool {
    // Not with frame generation (`VICTORY_FRAMEGEN=1`): its frames' inputs
    // count as submitted once their lists reached the queue
    // (`mark_frame_preparations`), which this thread does.
    if crate::framegen::requested() {
        return false;
    }
    // Diagnostics: on and off while the title runs (`crate::ab`); a
    // submission sent here first waits for those published.
    #[cfg(feature = "diagnostics")]
    if let Some(on) = crate::ab::pick("VICTORY_SUBMIT_SPLIT") {
        return on && relay_exact() && relay_copy() && send::own();
    }
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_SUBMIT_SPLIT").is_none_or(|v| v != "0") && relay_exact() && relay_copy() && send::own())
}


/// A published submission ([`split_send`]), for [`sender`].
pub(super) struct Handoff {
    /// Its number among the submissions started, and its fence value.
    pub(super) number: u64,
    pub(super) fence: u64,
    pub(super) lists: Vec<Option<ID3D12CommandList>>,
    /// Where each run's lists begin in `lists`, and the runs at which
    /// another part begins.
    pub(super) firsts: Vec<usize>,
    pub(super) parts: Vec<usize>,
    pub(super) take: Option<CardTake>,
    pub(super) flown: bool,
    pub(super) heaps: crate::descriptor::SyncedHeaps,
    /// Whether renderer port #7 asked for the heaps' round
    /// (`crate::port_journal::heaps_waited`).
    pub(super) heaps_on: bool,
    /// The slots its steps on the card fill ([`off_line`]).
    pub(super) slots: Option<card_steps::Slots>,
    /// Its lists' preludes, recorded by the sender ([`prelude_off_line`]).
    pub(super) preludes: Vec<PreludeWork>,
    /// The swap chain the sender presents behind it ([`present_off_line`]).
    pub(super) present: Option<IDXGISwapChain3>,
}

// SAFETY: D3D12 objects are free-threaded; a handoff is touched by the
// thread that publishes it and then by the sender alone.
unsafe impl Send for Handoff {}

/// With [`split_send`]: the submissions published and not taken by [`sender`]
/// yet, oldest first; how many were published, and how many sent.
pub(super) static SENDS: Mutex<(std::collections::VecDeque<Box<Handoff>>, u64, u64)> =
    Mutex::new((std::collections::VecDeque::new(), 0, 0));
pub(super) static SENDS_MOVED: std::sync::Condvar = std::sync::Condvar::new();

/// The fence value of the last submission [`sender`] sent.
pub(super) static SENT_FENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Since the last frame-rate line, of [`sender`]: submissions sent,
/// microseconds it worked, of them waiting for landings; microseconds other
/// submissions waited for it ([`wait_sent`]).
pub(super) static SEND_STATS: [std::sync::atomic::AtomicU64; 4] = [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// A submission is published ([`split_send`]): the sender sends it.
pub(super) fn publish(handoff: Box<Handoff>) {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        if let Err(e) = std::thread::Builder::new().name("victory-send".into()).spawn(sender) {
            fail(format_args!("no thread to send prepared submissions (VICTORY_SUBMIT_SPLIT): {e}"));
        }
        tracing::info!("submissions are sent by a thread of their own, the next one prepared meanwhile (VICTORY_SUBMIT_SPLIT=0: not)");
    });
    let mut sends = SENDS.lock().unwrap_or_else(|e| e.into_inner());
    sends.0.push_back(handoff);
    sends.1 += 1;
    drop(sends);
    SENDS_MOVED.notify_all();
}

/// Waits until [`sender`] has sent every submission published so far.
pub(super) fn wait_sent() {
    let mut sends = SENDS.lock().unwrap_or_else(|e| e.into_inner());
    if sends.1 == sends.2 {
        return;
    }
    let waiting = std::time::Instant::now();
    let _waiting = crate::timeline::span("waiting for published submissions to be sent");
    while sends.1 != sends.2 {
        sends = SENDS_MOVED.wait(sends).unwrap_or_else(|e| e.into_inner());
    }
    SEND_STATS[3].fetch_add(waiting.elapsed().as_micros() as u64, Ordering::Relaxed);
}

/// Whether the present's copy into the back buffer is published, and
/// [`sender`] makes the swap chain's Present behind it (on by default with
/// [`split_send`] and presents sent ahead, [`late_free_present`];
/// `VICTORY_PRESENT_OFF_LINE=0`: the present's thread sends the copy and
/// presents, as before).
///
/// Why: a submission that thread sends itself goes behind every one
/// published, so it waited for the sender ([`wait_sent`]): at the present,
/// 0.9 times and 1.42 ms a frame in the Series S cell (noop-mc3-tl), the
/// only such wait there; 1.0 to 1.35 ms with the preludes recorded by the
/// sender (noop-prel-ab1, noop-prel-ab2).
///
/// Who presents: the sender, behind the copy's lists and the signal of its
/// fence value, so the Present is behind the copy on the queue as before.
/// - The swap chain is called by one thread at a time. The sender calls
///   only Present. Every other call (`GetCurrentBackBufferIndex`,
///   `GetBuffer`, `ResizeBuffers`; the overlay and the panel make none, and
///   a window resized by the title keeps its buffers) is made under the
///   presenter's lock by a thread that first waited until every present
///   published was made ([`wait_presented`]).
/// - The back buffer a copy is recorded into is the current one, which
///   moves at a Present: that same wait.
/// - At most one present is published and not made: the next one waits for
///   it. That and the Present itself, which blocks the sender at DXGI's
///   frame latency as it blocked the present's thread, hold that thread
///   back: it runs ahead at most one present past where the Present held it
///   before. The flights in the air are held to [`late_most`] as before.
/// - The window: the title's, made and pumped by the title's thread that
///   called `CreateWindowExW` (victory-hle's `lifecycle`), or ours, made and
///   pumped by "victory-window", which waits for nothing. The swap chain is
///   windowed (DXGI's Alt+Enter off; full screen is the title's window
///   restyled by its own thread), so a Present makes no mode change. The
///   title's threads waited for the present's thread at presents and for
///   the sender at fences of published flights before this as well.
pub(super) fn present_off_line() -> bool {
    crate::toggled!("VICTORY_PRESENT_OFF_LINE") && split_send()
}

/// Presents [`sender`] made ([`present_off_line`]).
pub(super) static PRESENTS_MADE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Waits until [`sender`] has made `n` presents ([`present_off_line`]).
pub(super) fn wait_presented(n: u64) {
    if PRESENTS_MADE.load(Ordering::Acquire) >= n {
        return;
    }
    let waiting = std::time::Instant::now();
    let _waiting = crate::timeline::span("waiting for the last present to be made");
    // The sender counts a present before it counts its submission sent,
    // which it does under this lock.
    let mut sends = SENDS.lock().unwrap_or_else(|e| e.into_inner());
    while PRESENTS_MADE.load(Ordering::Acquire) < n {
        sends = SENDS_MOVED.wait(sends).unwrap_or_else(|e| e.into_inner());
    }
    SEND_STATS[3].fetch_add(waiting.elapsed().as_micros() as u64, Ordering::Relaxed);
}

/// The thread that sends published submissions ([`split_send`]), in order.
pub(super) fn sender() {
    use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL};
    // SAFETY: this thread's own priority.
    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL) };
    crate::cores::keep_on_fast_cores("victory-send");
    // It does work the line of the title's submissions waits for.
    crate::queue::no_drain_here();
    let Some(host) = host() else {
        return;
    };
    let ended = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        loop {
            let handoff = {
                let mut sends = SENDS.lock().unwrap_or_else(|e| e.into_inner());
                loop {
                    if let Some(handoff) = sends.0.pop_front() {
                        break handoff;
                    }
                    sends = SENDS_MOVED.wait(sends).unwrap_or_else(|e| e.into_inner());
                }
            };
            host.send_published(*handoff);
            SENDS.lock().unwrap_or_else(|e| e.into_inner()).2 += 1;
            SENDS_MOVED.notify_all();
        }
    }));
    if ended.is_err() {
        // Nothing published would ever be sent, nor land.
        fail(format_args!("the thread that sends prepared submissions panicked (VICTORY_SUBMIT_SPLIT)"));
    }
}


impl Host {
    /// [`sender`]: the rest of [`Host::submit`] for a published submission
    /// ([`split_send`]), without the submission lock.
    pub(super) fn send_published(&self, mut h: Handoff) {
        let began = std::time::Instant::now();
        #[cfg(feature = "diagnostics")]
        let split = Split::begin_sending();
        let mut stage = crate::timeline::span("sending: copies in the card's memory");
        // The compare's results start from the beginning only when no plan
        // is out: until the flights sent so far have landed (those published
        // behind them have none yet, and only this thread sends them).
        if h.flown && self.card_results_low() {
            let sent = SENT_FENCE.load(Ordering::Relaxed);
            let waiting = std::time::Instant::now();
            relay_wait(WHY_MOST, |line| {
                !line.settling && !line.line.iter().any(|l| matches!(l, Late::Run(f) if f.fence <= sent))
            });
            SEND_STATS[2].fetch_add(waiting.elapsed().as_micros() as u64, Ordering::Relaxed);
        }
        let card = h.take.take().and_then(|take| self.card_record(take, None, h.flown));
        let compare_at = card.as_ref().and_then(|c| c.check.clone()).map(|check| {
            h.lists.push(Some(check));
            h.lists.len() - 1
        });
        // The plan goes with its flight before the card can reach it.
        if let Some(card) = card {
            let mut line = line();
            match line.line.iter_mut().find_map(|l| match l {
                Late::Run(f) if f.number == h.number => Some(f),
                _ => None,
            }) {
                Some(flight) => flight.card = Some(card),
                None => fail(format_args!("a published submission is not in the line (VICTORY_SUBMIT_SPLIT)")),
            }
        }
        #[cfg(feature = "diagnostics")]
        split.part(4);
        // The lists' preludes are lists of the submission: recorded and
        // closed before any is sent.
        if !h.preludes.is_empty() {
            stage.next("sending: the lists' preludes");
            h.preludes.drain(..).for_each(PreludeWork::record);
        }
        stage.next("sending: waiting for the heaps' records");
        let heaps_began = std::time::Instant::now();
        drop(h.heaps);
        crate::port_journal::heaps_waited(heaps_began.elapsed().as_nanos() as u64, h.heaps_on);
        // The slots that waited for a texture's host copy, before the
        // lists that may read them run.
        crate::descriptor::make_waiting();
        // The slots its steps on the card fill, after the records.
        if let Some(slots) = h.slots.take() {
            stage.next("sending: the slots steps on the card fill");
            slots.write(true);
        }
        #[cfg(feature = "diagnostics")]
        split.part(15);
        stage.next("sending: the lists");
        // When it runs on the card, for the timeline.
        #[cfg(feature = "diagnostics")]
        let stamp = stamps::take();
        #[cfg(feature = "diagnostics")]
        let compare_at = if let Some((ahead, behind)) = stamp.and_then(stamps::lists) {
            h.lists.insert(0, Some(ahead));
            h.lists.push(Some(behind));
            h.firsts.iter_mut().for_each(|first| *first += 1);
            compare_at.map(|at| at + 1)
        } else {
            compare_at
        };
        #[cfg(feature = "diagnostics")]
        let sending = crate::timeline::ticks();
        #[cfg(feature = "diagnostics")]
        let compare_at = if let Some([front, behind]) = self.span_lists() {
            h.lists.insert(0, Some(front));
            h.lists.push(Some(behind));
            h.firsts.iter_mut().for_each(|first| *first += 1);
            compare_at.map(|at| at + 1)
        } else {
            compare_at
        };
        // SAFETY: closed lists; the fence is ours, and its value was given
        // under the submission lock, in the order of this thread's sends.
        let signalled = unsafe {
            for batch in submission_batches(&h.lists, &h.firsts, &h.parts, compare_at) {
                self.queue.ExecuteCommandLists(batch);
            }
            self.queue.Signal(&self.fence, h.fence)
        };
        awake::sent(&self.device);
        #[cfg(feature = "diagnostics")]
        crate::timeline::sent(sending, h.fence, stamp);
        self.card_sent();
        if let Err(e) = signalled {
            fail(format_args!("a submission sent ahead could not be followed by a signal ({e})"));
        }
        SENT_FENCE.store(h.fence, Ordering::Relaxed);
        #[cfg(feature = "diagnostics")]
        split.part(5);
        // The present behind its copy.
        if let Some(chain) = h.present.take() {
            stage.next("sending: the host's Present");
            // SAFETY: our swap chain; no other thread calls it until this
            // present is counted made ([`present_off_line`]).
            let _ = unsafe { chain.Present(0, DXGI_PRESENT(0)) };
            PRESENTS_MADE.fetch_add(1, Ordering::Release);
        }
        SEND_STATS[0].fetch_add(1, Ordering::Relaxed);
        SEND_STATS[1].fetch_add(began.elapsed().as_micros() as u64, Ordering::Relaxed);
    }
}
