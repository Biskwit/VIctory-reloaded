# What VIctory-reloaded changes, with its measurements

Every change of ours on top of Vice-Tay, as it went in: what it does, the
switch that turns it off, and what it measured, in the prologue's cell on an
i9-12900K with an RTX 4090 unless it says otherwise. The
[README](../README.md) has the summary; this is the record.

## The changes

- **Vice-Tay's tree, whole.** Its status, measurements, known problems, set-up
  and `scripts\play.ps1` apply here as they are: they are in
  [Vice-Tay's README](https://github.com/TaynanGT/Vice-Tay#readme), and its
  `docs` folder is unchanged.
- **Our layer**, a few commits on top, each naming the commits of our earlier
  tree it comes from:
  - the overlay (below), in every build;
  - pipelines with a static comparison sampler kept out of the pipeline
    library, as our earlier tree did: the driver's stored copies sample
    without the comparison (`VICTORY_LIBRARY_COMPARISON=1` stores them as
    before);
  - on NVIDIA only pipelines with static samplers skip the pipeline library
    (Vice-Tay fa8fc1d4): warm launches that loaded every pipeline from it
    drew the flat bands of colour listed below in 5 of 6 launches, and the
    driver's stored copies of static-sampler pipelines were what went wrong.
    Loading the others (about 950 of 1,800 in the cell) showed no banding
    and no pale hair in 6 warm Series S launches and 3 warm Series X ones;
    the cell comes about 10 s sooner (77 s median against 87), and frames
    are as before, as the static-sampler half is still made every launch.
    `VICTORY_LIBRARY_LOAD=0` loads none, `1` all, `samplers` this rule;
  - the write watch's asked ranges are a sorted index instead of a list
    walked at each look, and a submission's asking holds only its own
    pages: a few tenths of a millisecond to a millisecond on the submitting
    thread, not visible in the frame;
  - translated shaders carry a real shader hash part instead of zeros;
  - indirect dispatches of more than 65,535 groups run whole on NVIDIA too,
    as they did on AMD only: cut, the Series X cell scene showed rows of
    blocks at its bottom right (`VICTORY_INDIRECT_WIDE=0` cuts them). The
    dark stepped shape on the left wall and the speckles on the bunk that
    some launches show in that scene are not theirs: they come and go by
    launch with the cut too, and the shape shows on Series S with Vice-Tay's
    own tree and no ambient stand-in;
  - compute pipelines a dispatch asks for made first, and waited for within
    a cap, instead of dropped behind the draws' (`VICTORY_COMPUTE_FIRST=0`,
    `VICTORY_COMPUTE_WAIT`): cold launches kept the main character's head;
  - clears of unordered access views (`ClearUnorderedAccessViewUint` and
    `...Float`) are list methods; a clear did nothing before. The title
    clears two things that way, once, when the world begins: FSR4's 21 MB
    scratch block and an 80x45 texture with a value per 16x16 tile of the
    picture. What they held instead is read: with bytes 0xff in them every
    frame is white (`VICTORY_CLEAR_UAV=0`: left out, as before;
    `=poison[:<byte>[:buffer|:texture]]`: that byte instead, diagnostics).
    Still open: in about 6% of launches the cell comes up with a dark,
    stepped shape on its left wall (9 of 154 launches without the clears,
    1 of 14 with them: the clears are not its cure), and once in those 14
    the whole interior was dark instead; either state holds for the whole
    launch. What the title clears
    is read at least once at the world's start (bytes 0xff in the tile
    texture alone leave the picture white), but which pass reads it and
    what keeps the per-launch state is not found;
  - the control file, live switches, the texture view and window shots
    (diagnostics builds);
  - PIX captures taken from inside the run (diagnostics builds);
  - Series S speed on a PC with an i9-12900K (24 logical processors: 8 fast
    cores of two threads, 8 slow ones) and the RTX 4090, measured in the
    prologue's cell with frame replays and launches (table below):
    - title buffers from 64 KiB up get a copy in the card's own memory, not
      from 1 MiB (`VICTORY_CARD_BUFFERS_MIN_KB=64` in `play.ps1`'s 4090
      block): in the PC's memory every load a shader makes of them crosses
      the bus. A frame replay spent 34.7 ms on the card, 24.3 with them on it;
      live (on a trial merge of Vice-Tay's newer main) the card was busy 17 ms
      less a frame, for 0.4 GB of its memory;
    - fewer worker threads where the processor has fewer than 32 logical
      ones (8/6/3 list, under-write and landing threads instead of 12/8/6):
      more of them than the fast cores run made the submitting thread, which
      a frame waits for, share its core; 58.0-60.4 -> 54.3-56.1 ms a frame on
      that trial merge;
    - `XMemAllocDefault` blocks of 1 MiB and more are write-watched memory of
      their own, not the host heap's (`VICTORY_XMEM_WATCHED=0`: as before): a
      21 MB block that shaders write was copied whole twice a frame (42 MB,
      4.4 ms on the frame's path) and sent up to the card as often;
    - the frame replay makes the buffers of heaps opened from memory and of
      heaps made not resident (it lost the card on this capture), and with
      `--breadcrumbs` names the list, breadcrumb and resource a lost card
      stopped at;
    - windows of title memory in the card's memory, written by the CPU
      through resizable BAR (Vice-Tay bd68f5e's `VICTORY_WINDOWS_BAR_MB`, at
      most 6 GB in `play.ps1`'s 4090 block; a card with a budget under 20 GiB
      or without resizable BAR keeps them in the PC's memory): the card was
      busy 27-32 ms a frame, 18-21 with them, for about 5 GB more of its
      memory. Measured in the cell only; a large outdoor scene is untested;
    - what FSR4's passes write to their scratch, one 21 MB block every frame,
      stays on the card (`VICTORY_LAND_NOT`, their 20 programs listed in
      `play.ps1`): the title's CPU never reads it (a diagnostics probe,
      `VICTORY_LAND_NOT_PROBE=1`, saw no access over a launch), and it was a
      third of what the relay fetched and merged into title memory. One launch
      switching it on and off: 52.8 -> 49.7 ms a frame, landing 25.2 -> 18.2;
    - the title's writes into guarded memory go in beside the guard
      (`VICTORY_STORE_BESIDE`, `VICTORY_COPY_BESIDE`, both existing switches,
      now on in `play.ps1`'s 4090 block): write faults that open a block
      fell from about 300 to 200 a frame, and all threads' waits for the
      guards' lock from 54-60 to 32-36 ms a frame, most of it the title's own
      threads'. The frame does not move yet: the submitting thread is the
      wall. Diagnostics builds name where that lock is held longest;
    - a write fault on a block whose page is shown at one address only opens
      it under a gate of its own, not the guards' lock
      (`VICTORY_FAULT_FAST=0`: the lock, as before): about 72% of the faults
      that open a block take that path, and all threads' waits for the lock
      fell another 55% (28-40 -> 12-18 ms a frame). Two races in it that could
      leave a write unreported were found by a stress test and fixed; each
      has a test that holds the opener at the racy point (20 of 20 failed
      without the fix, 0 of 20 with it);
    - a guard-page fault inside one of our own copies re-arms the page
      (Vice-Tay fa116500 + c2775339): it used to leave the title's guard page
      (a thread's stack, for one) without its guard, which upstream traced to
      silent exits a few minutes into the world;
    - from Vice-Tay's newer main, translator and landing fixes: the console
      wave64 programs `play.ps1` listed are translated in the wave-wide form
      on a 32-lane card (538bf963, 37e9f348, 0367bf95). Before, nothing read
      that list and two of them, the cell's volumetric transmission passes,
      were dropped every frame; drawn, they change nothing measurable in the
      cell, and their picture elsewhere is not checked yet. Also typed
      stores keep their integer bits (189 programs change), four tiled
      shadow programs take the exact form instead of an approximate one,
      one interpolation mode per signature row (a fire program now
      validates), a level query without an array slice, the relabelled
      wave's group info, the card-copy compare in a call of its own, and the
      first piece of a parallel compare kept (6f179dd1, b4c3ceb2, 4fd34fdd,
      0dc5d1be, 41d3184e, 53c9ab5d, fa8fc1d4, 21ec8110);
    - our threads kept off the processors the title pins its own to (an HLE
      SetThreadGroupAffinity learns its masks; here it takes logical
      processors 0-13): the eight list threads go to the slow cores it
      leaves, the submitting thread to the fast ones it leaves
      (`VICTORY_PLACE_APART=0`, `VICTORY_PLACE_LANE=0`: as before). In-run
      2x2 over 3 launches: 42.3 -> 40.0 ms a frame with both; putting every
      pool on the slow cores instead cost 5 ms, as threads the submitting
      one waits for slowed down. Its gate ran at 38.3-41.9 ms a minute (about
      25 fps). Also from Vice-Tay: a batch wakes only the helpers it can use
      (d60fc84f), and the frame meter's 5-second lines run on a thread of
      their own (f36a0fcf), about 30 ms every 5 s off the submitting thread;
    - the ambient stand-in reads the title's depth: where it looked for depth
      the title binds the stencil plane of its depth texture, so its creases
      and depth-weighted interpolation worked from stencil bits. It now finds
      the depth plane among the program's views (`VICTORY_AMBIENT_DEPTH=0`:
      the old view), and binds depth only, no longer an image's UAV in an SRV
      table (Vice-Tay fa8fc1d4). In-run A/B: the cell's wall, the whole frame
      and the papers' colour each move slightly toward the reference, at the
      same frame time;
    - more of Vice-Tay's newer fixes: a new texture waits for the lists sent
      before it (`VICTORY_SHOW_NEW_LATER`, 4cd2d133, 91018a5d; upstream
      traced six device losses to this), a decoration bake's texture pass is
      known per heap (7eef1436), dynamic slots go back to their own heap
      (3fe48113, 5307ba7d), an upload no longer stops an alias copy
      (7993c1bc), and a full texture-change queue keeps its ranges
      (53a1f162);
    - a submission looks only at the card copies it touched or overlaps
      for what changed, not at every copy in the card's memory (about 3,700;
      `VICTORY_CARD_NEAR=0`: all, as before): the submitting thread's card
      copies 1.6-1.7 ms shorter, the frame 0.7-1.0 ms in-run; its gate ran at
      about 25 fps (25.8 over 12 minutes);
    - the card steps' slot writes of a submission are done by the sending
      thread after the heaps' records it waits for, on their pool, and empty
      host lists are closed where they are reset (`VICTORY_OFF_LINE=0`: on
      the submitting thread, as before). In-run over 3 launches 39.7 -> 38.0
      ms a frame; the title's wait at presents 14.5 -> 11.8. A slot now gets
      its fill after the title's records rather than before, as on the
      console; in the cell no slot is both (a diagnostics count,
      `VICTORY_SLOT_ORDER_COUNT=1`). Its gate ran at 37.8-40.0 ms a frame;
    - the title's words in card blocks that shaders in flight may write go
      up by mask, a bit a word for each 4 KiB, and a pass on the card stores
      the words of set bits alone (`VICTORY_UNDER_MASKS=0`: as runs of
      words, as before). They were about 100,000 runs a frame, most in the
      title's 174 MB heap, which a view of the skinning passes declares
      written whole 8.7 times a frame. In-run over 2 launches 37.8 -> 36.0
      and 38.1 -> 36.6 ms a frame; a check launch each way found no copy
      stale after a refresh. Its gate ran at 37.0 ms over 12 minutes;
    - a host copy keeps a stamp of the range a look found up to date, which
      every write the journal notes on its pages takes away: a refresh its
      stamp holds is left out at once, with no lock taken
      (`VICTORY_REFRESH_STAMPS=0`: looked at each time, as before). Of some
      10,500 refreshes a frame, about 9,000 copied nothing. In-run over 2
      launches 36.9 -> 36.4 and 38.1 -> 37.6 ms a frame; a check compared
      about 1.7 million skipped refreshes with title memory and none
      differed;
    - a submission's steps are walked once for what to ask the write watch,
      what to refresh and what lies under flights' writes, and the lists'
      steps take that walk's verdicts instead of looking at the stamps again
      (`VICTORY_ONE_WALK=0`, `VICTORY_STALE_WALKED=0`: as before). In-run
      over 2 launches each: -0.55 and -0.60 ms a frame for the one walk,
      -0.40 and -0.35 more for the copies under flights' writes. Its gate
      ran at 35.8 ms (27.9 fps) over 4 launches and 34.0-35.1 ms a minute
      over 12 minutes;
    - the preludes of a submission's lists (the texture transitions and
      twin copies a list needs before it runs) are recorded by the sending
      thread; what they hold is still decided by the submitting thread, in
      submission order (`VICTORY_PRELUDE_OFF_LINE=0`: as before). In-run
      over 2 launches: 35.7 -> 35.2 and 35.4 -> 34.85 ms a frame, the
      title's wait at presents 9.1 -> 8.25 and 9.3 -> 7.65. Its gate ran at
      35.0-36.5 ms a frame and 34.3-35.5 ms a minute over 12 minutes;
    - the plan of a submission's steps on the card, made when deciding
      whether to cut it, is kept for the same submission's steps instead of
      made again (`VICTORY_PLAN_ONCE=0`: planned twice, as before), and the
      card work is found ready without its lock: the submitting thread's
      card steps 0.5 ms shorter in 3 launches. The frame moves less than
      the noise between identical arms (about half of a saving on that
      thread reaches the frame now); its gate ran at 34.4 ms over 12
      minutes;
    - the present's copy into the back buffer is sent by the sending
      thread like the other submissions, and that thread also makes the
      swap chain's Present behind it; every other call on the swap chain
      first waits until the presents handed over are made, and at most one
      is in the air (`VICTORY_PRESENT_OFF_LINE=0`: the present's thread,
      as before). The submitting thread no longer waits for the sending
      one at each present: in-run over 2 launches 35.9 -> 35.0 and 36.2 ->
      35.5 ms a frame, the title's wait at presents 8.4 -> 6.65 and 9.0 ->
      7.7. Its gate ran at 35.5-36.2 ms a frame; the frame's chain now
      holds about 8 ms of the title's own work;
    - the write watch's journal keeps its pages in 4 MiB pieces instead of
      one tree: a look at a range, some 10,000 a frame, takes 20 ns instead
      of 111; the submitting thread about 1 ms less a frame;
    - a submission is prepared while the one before it is sent: the line's
      thread prepares and publishes it, a second thread (victory-send)
      records the card-copy lists, waits for the heaps' records and sends
      (`VICTORY_SUBMIT_SPLIT=0`: one thread, as before). In-run A/B over 4
      launches: 53.1 -> 50.5 ms a frame, the title's wait at presents 19.9 ->
      16.0 ms; its gate ran at 43.3-46.0 ms a frame (23 fps over a 15-minute
      session).
- **Not brought over (yet)** from our earlier tree (fxceless's fork with about
  140 commits of ours): its FidelityFX SDK path for the Series S upscale
  (Vice-Tay translates FSR 4's own code) and its other rendering fixes, which
  are being checked one by one against Vice-Tay's picture. Nor its
  `victory replay-dispatch` (one compute dispatch captured and run again with
  other compiles, the pipeline library's stored binary, other samplers): it
  reaches into the translator, the pipeline library and the recording paths
  Vice-Tay changes most, its module name is taken by Vice-Tay's frame replay,
  and Vice-Tay's `victory replay` ([docs/replay.md](replay.md)) already
  re-translates, swaps and skips shaders and finds what draws a pixel.

## Measurement history

i9-12900K, RTX 4090, 64 GB, `scripts\play.ps1` (its 4090 block), the
prologue's cell, warm launches; medians of the log's frame lines once the
scene has settled (frames 1300-3600). Vice-Tay's own measurements, on its
Ryzen 9 9950X3D, are below the line.

| Build | fps: median (p10) | ms a frame | preparing / card busy / copying back |
|---|---|---|---|
| before (4fa2f0b), 2 launches | 17.4 (16.7) | 57.5 | 45.5 / 43.8 / 24.1 |
| after (the three changes above), 3 launches | 19.9 (19.0) | 50.3 | 40.0 / 30.8 / 26.5 |
| after, the gate: 1 cold launch on a fresh cache and 3 warm ones | 19.8 (18.8) | 50.5 | 40.7 / 31.0 / 25.8 |
| after, the gate's 15-minute warm session | 20.9 (19.9) | 47.8 | 37.9 / 29.6 / 21.5 |

The next round (2026-11-27): the levers listed above from the windows in the
card's memory on. Measured on a quiet machine, one build after the other,
each with its own `play.ps1` settings and a warm-up launch first:

| Build | fps: median (p10) | ms a frame | preparing / card busy / copying back |
|---|---|---|---|
| before (7cae3c5), 3 warm launches | 19.7 (19.2) | 50.7 | 40.8 / 28.9 / 24.0 |
| after (0ae83d16), 3 warm launches | 27.0 (25.9) | 37.1 | 30.1 / 20.3 / 18.2 |
| after, a 10-minute session | 26.9 (26.2) | 37.2 | 30.2 / 22.0 / 17.6 |

The session held 36.5-38.0 ms a frame minute by minute, with the game's
private memory flat at 24.4-25.6 GB. The frame is still set by the thread that
prepares the submissions; the card is busy about 20 ms of it.

The same settings on Series X, in the same cell: 73.0-80.7 ms a frame before
(12.4-13.7 fps, 2 launches of 7d190fe), 57.6 ms after (17.4 fps, 1 launch);
card busy 65-70 -> 47 ms. After the second round, 51.8-55.1 ms (about 19 fps,
3 warm launches with the pipeline library).

The third round (2026-11-28): the masks, stamps, one walk, preludes, plan and
present above. Measured the same way, the two builds' launches taking turns
after a warm-up launch of each:

| Build | fps: median (p10) | ms a frame | preparing / card busy / copying back |
|---|---|---|---|
| before (0ae83d16), 3 warm launches | 26.3 (24.3) | 38.0 | 30.1 / 21.3 / 17.6 |
| after (36dc2028), 3 warm launches | 28.8 (26.9) | 34.7 | 25.1 / 20.2 / 17.4 |
| after, a 12-minute session | 29.6 (28.6) | 33.8 | 24.9 / 20.5 / 16.1 |

The session held 33.3-34.6 ms a frame minute by minute from its second minute,
with the game's private memory between 24.8 and 26.9 GB. On Series X, 2 warm
launches of 36dc2028: 21.1 fps (p10 18.8), 47.4 ms a frame; preparing 25.5,
card busy 33.3, copying back 29.4 ms.
