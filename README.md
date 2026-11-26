# VIctory-reloaded

A work-in-progress fork of [VIctory](https://github.com/Marvisak/VIctory), the
compatibility layer that runs the Xbox Series build of Grand Theft Auto VI
natively on Windows from files the user supplies. VIctory-reloaded builds on
[fxceless's fork](https://github.com/fxceless/victory-fork) and is aimed at
NVIDIA GPUs: the opening of the game now renders as it should, and the fork
adds a toolkit for finding the rest of what is wrong.

**This repository contains no game files, code or assets.** You need your own
copy of the game. It is not affiliated with or endorsed by Rockstar Games or
Take-Two Interactive.

![The prologue's first gameplay shot](https://github.com/Biskwit/VIctory-reloaded/releases/download/v0.1.0-preview/prologue-cell-window.png)

*The prologue's first gameplay shot on an RTX 4090. The picture is hosted as
a release attachment, outside the repository: by the
[asset policy](docs/asset-policy.md) nothing from the game goes in the tree.*

## Status (2026-11-26)

**Not playable yet: about 6 frames per second.** Tested on one machine:
Windows 11, Intel i9-12900K, NVIDIA RTX 4090 24 GB, 64 GB RAM, Series X
profile.

| Stage | State |
|-------|-------|
| Boot, menus, start screen | works, 30 fps |
| Opening cinematics | play in order, lit |
| Prologue, first gameplay shot (the cell window) | renders like the console: lighting and shadows, hair, the orange jumpsuit, the frosted window with the fence and the yard behind it; the same look on every launch, first launch or not |
| Frame rate in the world | about 6 fps (160-180 ms a frame), limited by the CPU side of the GPU emulation |
| First launch after a translator change | slower to load: shaders are translated and pipelines compiled, and a few loading frames last several seconds; later launches load from the caches |
| Stability | no crash or device loss in the test runs |

### What this fork changes

Rendering:
- **Lighting that changed from launch to launch.** It had two causes. The driver's saved copies of pipelines with comparison samplers lost the comparison, so shadows vanished on the next launch; such pipelines are now never stored. And a persistent sky-light compute chain was dropped for minutes while it waited in the pipeline queue. Now a pipeline a dispatch waits for is made first, and dispatches wait for their pipeline (capped) instead of being dropped.
- **Pipelines kept out of the library are no longer started at boot,** where they held every compile worker for minutes and starved the first world frames (the dark jumpsuit).
- **Compute dispatches of more than 65,535 groups run whole.** Before, the rest were cut, leaving blocks of other textures at the bottom of the picture.
- Also fixed:
  - textures packed by compute shaders and read through views of another format are copied every frame;
  - depth-bounds tests;
  - stale views of textures in shared memory;
  - descriptor records the title copies as bytes;
  - translator gaps: 64-lane wave operations on 32-lane hosts, half-precision wave sums, lane masks used as numbers.
- **Ray tracing is still off by default.** The builder (`VICTORY_BVH_BUILDER=1`) runs, but its memory grows without bound.

Speed:
- Large title buffers live in card memory, with changes found by hashing on the card.
- Submissions overlap the card through a relay, and the relay fetches results on its own copy queue.
- Mesh draws go through an amplification stage instead of a clamp pass of ours.
- Translated shaders are keyed by the translator's sources, so a rebuild of the rest keeps the shader cache.

On screen:
- **Overlay on the game, in every build.** On by default (`VICTORY_OVERLAY=0` hides it): a compact table at the top left, in Consolas from Windows, with the console profile (Series X or S), frames per second and time per frame (green from 15 fps, amber from 10, red below), where the frame's time goes (CPU prep, GPU, copy-back), draws, dropped draws and VRAM.

Debugging (diagnostics builds, see below):
- **Control file to steer a running game:** live switches, screenshots, render-target dumps, viewing any render target on screen, repairs, PIX captures.
- **PIX captures from inside the run:** `VICTORY_PIX_CAPTURE` or the control file.
- **Capture one compute dispatch and replay it offline** (`victory replay-dispatch`). It can rerun the dispatch with other compiles, with the exact binary a launch stored, with other samplers, or with re-translated code, and report which inputs a difference comes from. This is how the lighting bug was found.

### Known problems

- At the cell window, the frosted glass and the fence are drawn over the character's hair.
- About one launch in seven shows a dark band across the character's back on some frames.
- Ray-traced bounce light and shadows are off (see above).
- About 6 fps; the CPU side dominates.
- Many changes are specific to this title and this GPU; those that approximate the console are marked `shortcut:` in the code.

## Credits

- **[VIctory](https://github.com/Marvisak/VIctory)** by Marvisak and its
  contributors is the base of everything here: the loader, the runtime
  services, the Direct3D 12 back end and the shader recompiler. Licensed
  GPL-3.0-or-later; so is this fork.
- **[fxceless's fork](https://github.com/fxceless/victory-fork)** (formerly
  victory-gta6), which this fork started from, and whose later fixes (depth
  bounds, texture re-checks, the stale-texture rule, descriptor mirroring)
  were ported here commit by commit with credit.
- **[XCL](https://github.com/ivall/xcl)** by ivall and its contributors
  (GPL-2.0-only) was used by fxceless's fork as a reference for how the same
  title behaves under another layer.

Below this line is upstream VIctory's README, with this fork's commands and
switches added to its Commands and Debugging sections. Its status section
describes upstream, not this fork.

---

# VIctory

Runs Grand Theft Auto VI (Xbox Series X|S) natively on Windows, from files
the user supplies.

The title's x86-64 code runs unchanged inside our process; VIctory supplies
the platform underneath it and converts the title's data, ahead of time where
it can and on first use where it must (shaders). See
[docs/architecture.md](docs/architecture.md).

## Status (2026-11-23)

**Not playable yet.** The title boots, runs its menus at its own 30 fps and
loads the world, where rendering fails.

| Stage | State |
|-------|-------|
| Boot, first-boot screens (accessibility, language, settings, legal) | works, 30 fps |
| Intro video with sound | works |
| Start screen, profile and saves | works |
| Loading into the world (about 50 s) | works |
| The world (first mission's intro) | reached and runs with indirect draws; its title card renders, the scene after it is still dark; 0.5-1.8 s per frame |

Tested on one machine: Windows 11, AMD RX 6700 XT.

> **Warning.** Entering the world on a real GPU can currently hang it, and
> Windows then resets the display driver (every monitor goes black for a
> moment). To look further, use a diagnostics build with
> `VICTORY_D3D12_WARP=1`, which renders on the software adapter.

What each part does today and what it still needs:

| Subsystem | Today | Needs |
|-----------|-------|-------|
| Loader | Runs the title's executable and its DLLs in our process; crash reports name the faulting code and are kept in `crashes.log` | â€“ |
| Runtime services | One signed-in offline user, licences, packages, saves that survive a kill at any step, text entry dialog | Speech is silent; the dialog and relaunch are untested live |
| Network | Offline by design: the title cannot reach the network | â€“ |
| Files and streaming | Archives and streamed data read, zlib and BCPack decoded in parallel | Loading is limited by writes into title memory |
| Input | XInput pads, keyboard, keyboard stand-in pad | Trigger rumble |
| Audio | Menu, video and loading sound; XMA voices and Opus streams decode | Reverb is silent; Opus results unverified against the title |
| GPU: menus | Draws, compute and presentation on host D3D12; only changed title memory is copied | Frame pacing drifts on one screen in release builds |
| GPU: world | Mesh pipelines, shader-written buffers, descriptor lifetimes, indirect execution (arguments clamped), pipelines made by worker threads and kept on disk | Most of the scene is still missing (untranslated shaders drop draws); frames take seconds; real queries, depth bounds, HDR and scaling are missing |
| Shaders | Console GPU code recompiled to DXIL at first use, cached on disk. Menus: all. World: mesh 8,191 of 8,506, vertex/pixel 10,617 of 12,346, compute 1,643 of 2,539 | 64-bit image atomics, merged geometry stages, the ray-tracing builder's shaders; some compute programs need a GPU with 64-lane waves |
| Textures | Console tiled layouts and detiling for the modes the title uses | Render-target metadata planes |
| Series S mode | Runs through the menus to the loading screen | The world, as above |

## Getting started

Requirements: Windows 10/11 x64, a CPU with AVX2, a Direct3D 12 GPU with
shader model 6.6 (and mesh shaders for the world), Rust (pinned in
`rust-toolchain.toml`).

The title also needs several GiB of available Windows commit at startup.
Check **Committed** under **Task Manager â†’ Performance â†’ Memory**. If
`crashes.log` reports Windows error 1455, close other applications or increase
the Windows paging file size, then retry.

```powershell
cargo xtask install-hooks                # asset-policy check on every commit
copy victory.example.toml victory.toml   # then set your paths (outside the repo)
cargo test --workspace                   # needs no game data
cargo run -p victory -- config           # check your paths
```

Run the title:

```powershell
cargo build --release -p victory
target\release\victory.exe run <game dir>\<the title's exe> --execute
```

The window caption and the overlay show the framerate (`VICTORY_OVERLAY=0`
hides the overlay). Saves, settings and `crashes.log`
are kept under the data folder (default `%LOCALAPPDATA%\victory\data`), derived
data under the cache folder.

Input: XInput pads. With none connected the keyboard is a pad while the
title's window has focus:

| Keys | Pad |
|------|-----|
| W A S D / I J K L | left / right stick |
| arrows | D-pad |
| Enter, Backspace, X, Y | A, B, X, Y |
| Q / E, Z / C | shoulders, triggers |
| F / H | stick clicks |
| Esc, Tab | Menu, View |

## Commands

```
victory inspect <exe|dll> [--imports] [--exports]
victory census [paths...] [--missing] [--strict]
victory lib-exports <import.lib> [--list]
victory shader-info [paths...] [--verbose] [--disasm]
victory translate [paths...] [--validate] [--pipelines] [--list] [--jobs N]
victory run <exe> [--execute] [--unimplemented trap|zero] [--strict]
            [--console series-x|series-s] [-- <title arguments>]
victory config
victory replay-dispatch <dir> [--variants N] [--library <file>] [--dxil <file> | --retranslate]
            [--sampler s<reg>.<field>=<value>] [--sensitivity] [--diffs] [--warp]   (diagnostics builds)
```

Logging: `VICTORY_LOG=victory_loader=trace,info` (a release build logs
info and above).

## Debugging

A release build has no debugging aids. Build them in with

```powershell
cargo build --profile diag -p victory --features diagnostics   # target\diag\victory.exe
```

| Variable | Effect |
|----------|--------|
| `VICTORY_LOG=<target>=debug,...` | debug and trace output |
| `VICTORY_STACKS=20,60` | every thread's stack at those seconds |
| `VICTORY_WATCH=<rva>,...` | logs a title u32 when it changes |
| `VICTORY_KEYS=30:Enter,35:X` | presses keys at those seconds, focus or not |
| `VICTORY_CRASH_SAVE=<rva>:<len>,...` | saves title memory to the cache folder on a crash |
| `VICTORY_CAPTURE=<dir>` | saves presented frames (powers of two, or `VICTORY_CAPTURE_FRAMES=<list>`) |
| `VICTORY_DUMP_TARGETS=1` | with the two above, saves every render target of those frames |
| `VICTORY_D3D12_DEBUG=1` | host debug layer |
| `VICTORY_D3D12_GBV=1` | with the above, GPU-based validation (bad descriptor use reported, not faulted; slow) |
| `VICTORY_D3D12_DRED=1` | on a GPU failure, the allocation hit and the commands in flight |
| `VICTORY_SKIP_PIPELINES=<hash>,...` | omit draws and dispatches, including indirect ones, whose pipeline contains a listed 16-digit shader hash |
| `VICTORY_GPU_TIMING=1` | with the above, each list of a submission run and timed on its own; slow lists logged with their slowest commands |
| `VICTORY_D3D12_WARP=1` | host device on the software adapter (slow; cannot reset the driver) |
| `VICTORY_SHADER_LOOP_LIMIT=<n>` | translated shader loops stop after `n` iterations per lane instead of 2^20 (own shader cache entries); if a lower `n` makes world frames much faster, loops hitting the limit are what makes them slow |
| `VICTORY_AUDIO_CAPTURE=<file.wav>` | records what the title plays (our process only) |
| `VICTORY_XMA_DUMP=<dir>` | saves the first 32 XMA voices' context and input |
| `VICTORY_PIX=1` | loads PIX's capturer at the start, for `pix` below |
| `VICTORY_PIX_CAPTURE=<frame>:<file.wpix>` | PIX captures that frame (a run with the capturer never saves the pipeline library) |
| `VICTORY_DISPATCH_CAPTURE=<hash>[+<hash>...]:<frame\|w<n>>:<dir>` | saves what one compute dispatch needs to run again offline (`w<n>`: n frames into the world, with `VICTORY_CAPTURE_WORLD=1`); see `replay-dispatch` |
| `VICTORY_DROP_NAMES=1` | names the pipelines whose draws were dropped while they were made |
| `VICTORY_CONTROL=<file>` | the control file (default `D:\victory-control.txt`) |

### Steering a running game

A diagnostics build looks at the control file at every present; when it
changes, its commands run once (one a line, `#` for comments) and the file
is deleted:

| Command | Effect |
|---------|--------|
| `overlay on` / `overlay off` | shows or hides the overlay (on by default); in these builds it adds a smaller block: dispatches, shader and pipeline caches, pipelines compiling, shared memory, the live switches set |
| `view <n> [r\|g\|b\|a\|rgb]` / `view <0xaddress> [...]` / `view off` | presents the frame's `n`-th target (first-use order, as `dump` numbers them) or a texture by address instead of the frame; floats and depth normalized as dumps are |
| `targets` | logs the next frame's targets: number, address, size, format |
| `shot [name]` | saves what the window shows (without the overlay) as `<name>.png` in `VICTORY_CAPTURE`, else beside the control file |
| `dump` | saves the next frame's targets, as `VICTORY_DUMP_TARGETS` does |
| `pix <file.wpix>` | PIX captures the next frame (needs `VICTORY_PIX=1`) |
| `set NAME=VALUE` / `unset NAME` | changes a live switch: `VICTORY_WIDE_DISPATCH`, `VICTORY_ALIAS_ONCE`, `VICTORY_HEAP_RECHECK`, `VICTORY_HEAP_SYNC`, `VICTORY_STALE_TEXTURES`, `VICTORY_TEXTURE_GUARD`, `VICTORY_SCISSOR_TEST`, `VICTORY_SKIP_PIPELINES`; other switches are read at the start |
| `repair textures` / `repair aliases` / `repair heaps` | uploads every title-filled texture again / redoes every alias copy / re-syncs every descriptor heap slot |

Every build appends crashes and host GPU failures to `crashes.log` in the
title's data folder.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
