//! `victory`: the command-line front end.
//!
//! Every command reads the user's own files and writes only to stdout or to
//! the user's cache directory (outside the repository).

mod census;
mod inspect;
mod run;
mod shader;
mod translate;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(
    name = "victory",
    version,
    about = "Run an Xbox Series title natively on Windows"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Summarise one PE image: headers, sections, imports, TLS, build id.
    Inspect {
        file: PathBuf,
        /// List every imported function.
        #[arg(long)]
        imports: bool,
        /// List every exported function.
        #[arg(long)]
        exports: bool,
    },
    /// Aggregate the imports of every PE image under the given paths and
    /// show which ones our runtime covers. Defaults to the configured
    /// game and test-title directories.
    Census {
        paths: Vec<PathBuf>,
        /// Do not count host pass-through as covered.
        #[arg(long)]
        strict: bool,
        /// List every missing `dll!function`, most used first.
        #[arg(long)]
        missing: bool,
    },
    /// List the `dll!export` pairs an import library describes.
    LibExports {
        lib: PathBuf,
        /// List every export, not just per-DLL counts.
        #[arg(long)]
        list: bool,
    },
    /// Find shader containers in files or directories (also inside engine
    /// archives, if uncompressed) and report their layout.
    ShaderInfo {
        paths: Vec<PathBuf>,
        /// Print one line per container found.
        #[arg(long)]
        verbose: bool,
        /// Also list the decoded native instructions (implies --verbose).
        #[arg(long)]
        disasm: bool,
    },
    /// Run every shader found (containers, SGD5 files) through the
    /// translator and report what it cannot handle yet.
    Translate {
        paths: Vec<PathBuf>,
        /// Check emitted DXIL with dxil.dll's validator (`VICTORY_DXIL`,
        /// else the newest Windows SDK's).
        #[arg(long)]
        validate: bool,
        /// Also create a host D3D12 pipeline for each emitted shader.
        #[arg(long)]
        pipelines: bool,
        /// Print one line per program and outcome.
        #[arg(long)]
        list: bool,
        /// Worker threads (default: every logical core).
        #[arg(long)]
        jobs: Option<usize>,
    },
    /// Run a compute dispatch captured from a run
    /// (`VICTORY_DISPATCH_CAPTURE=<shader hash prefix>:<frame>:<folder>`,
    /// diagnostics builds) again on this GPU, and compare what it writes
    /// with what it wrote in the run.
    ReplayDispatch {
        /// The capture's folder (its `manifest.json`).
        dir: PathBuf,
        /// Programs to run: the captured code, then translations of the
        /// captured shader with salts 1, 2, ... (each compiled anew by the
        /// driver); outputs that differ between them show code whose results
        /// depend on its compilation.
        #[arg(long, default_value_t = 1)]
        variants: u32,
        /// Runs of each program.
        #[arg(long, default_value_t = 2)]
        runs: u32,
        /// Run this DXIL container instead of the captured code.
        #[arg(long, conflicts_with = "retranslate")]
        dxil: Option<PathBuf>,
        /// Run the captured shader as this build translates it.
        #[arg(long)]
        retranslate: bool,
        /// Differing places listed per resource.
        #[arg(long, default_value_t = 4)]
        show: usize,
        /// Write a picture of where each texture output differs from the
        /// first run's into this folder.
        #[arg(long)]
        diffs: Option<PathBuf>,
        /// Change a static sampler of the root signature the programs are
        /// made with: `s<register>.<field>=<value>`, fields filter, u, v, w,
        /// func, border, aniso, bias, minlod, maxlod (repeatable).
        #[arg(long = "sampler")]
        samplers: Vec<String>,
        /// Compare with another replay's outputs instead of the capture's:
        /// the prefix of its `--diffs` files, e.g. `<dir>/program-0`.
        #[arg(long)]
        expect: Option<PathBuf>,
        /// Load each program from this pipeline library of a run
        /// (`host-pipelines.bin`) when it is there under its name: the
        /// driver's binary that run used. Compiled otherwise.
        #[arg(long)]
        library: Option<PathBuf>,
        /// Dispatch the groups this many at a time in x, each part from its
        /// first group (other co-scheduling of groups and waves; the same
        /// results for code without races).
        #[arg(long, default_value_t = 0)]
        split: u32,
        /// Zero each input in turn and run the first program again: which
        /// outputs change, and how much of that where the replay differs
        /// from the capture.
        #[arg(long)]
        sensitivity: bool,
        /// After each program's runs, have the driver commit its background
        /// work (it compiles shaders again after their first use) and run
        /// the program as many times again.
        #[arg(long)]
        background: bool,
    },
    /// Prepare, map and bind a title executable; with --execute, run it.
    Run {
        exe: PathBuf,
        /// What unimplemented imports do when called.
        #[arg(long, value_enum, default_value_t = Unimplemented::Trap)]
        unimplemented: Unimplemented,
        /// Disable host pass-through (everything must be implemented by us).
        #[arg(long)]
        strict: bool,
        /// Actually start the title. Without it, stop after binding.
        #[arg(long)]
        execute: bool,
        /// The console model the title sees (device type, memory).
        #[arg(long, value_enum, default_value_t = Console::SeriesX)]
        console: Console,
        /// Arguments for the title, after `--` (it reads the whole command
        /// line; a title that relaunches itself passes some).
        #[arg(last = true)]
        title_args: Vec<String>,
    },
    /// Show the resolved configuration and check it.
    Config,
}

#[derive(Clone, Copy, ValueEnum)]
enum Unimplemented {
    Trap,
    Zero,
}

#[derive(Clone, Copy, ValueEnum)]
enum Console {
    SeriesX,
    SeriesS,
}

fn main() -> Result<()> {
    victory_core::logging::init("info");
    // The audio recorder `run` starts for `VICTORY_AUDIO_CAPTURE`.
    #[cfg(all(windows, feature = "diagnostics"))]
    if victory_hle::modules::audio::capture::child() {
        return Ok(());
    }
    let cli = Cli::parse();
    match cli.command {
        Command::Inspect {
            file,
            imports,
            exports,
        } => inspect::run(&file, imports, exports),
        Command::Census {
            paths,
            strict,
            missing,
        } => census::run(paths, strict, missing),
        Command::LibExports { lib, list } => inspect::lib_exports(&lib, list),
        Command::ShaderInfo {
            paths,
            verbose,
            disasm,
        } => shader::run(paths, verbose || disasm, disasm),
        Command::Translate {
            paths,
            validate,
            pipelines,
            list,
            jobs,
        } => {
            // `VICTORY_VIEW_LIMIT=<null slot>`: translate as on a host whose view heap is capped
            if let Some(n) = std::env::var("VICTORY_VIEW_LIMIT").ok().and_then(|v| v.parse().ok()) {
                victory_shader::emit::set_view_heap_limit(n);
            }
            // `VICTORY_HOST_WAVE=32`: translate as for a GPU whose waves are at most 32 lanes
            if let Some(n) = std::env::var("VICTORY_HOST_WAVE").ok().and_then(|v| v.parse().ok()) {
                victory_shader::emit::set_host_max_wave(n);
            }
            // `VICTORY_MEMORY_PAGES=1`: translate as with title memory by address in page copies
            if std::env::var_os("VICTORY_MEMORY_PAGES").is_some_and(|v| v == "1") {
                victory_shader::emit::set_memory_pages(true);
            }
            translate::run(paths, validate, pipelines, list, jobs)
        }
        Command::ReplayDispatch {
            dir,
            variants,
            runs,
            dxil,
            retranslate,
            show,
            diffs,
            samplers,
            expect,
            library,
            split,
            sensitivity,
            background,
        } => {
            let options = Options {
                variants,
                runs,
                dxil,
                retranslate,
                show,
                diffs,
                samplers,
                expect,
                library,
                split,
                sensitivity,
                background,
            };
            replay_dispatch(&dir, options)
        }
        Command::Run {
            exe,
            unimplemented,
            strict,
            execute,
            console,
            title_args: _,
        } => run::run(&exe, unimplemented, strict, execute, console),
        Command::Config => show_config(),
    }
}

/// What `replay-dispatch` runs (`victory_gpu::replay::Options` on Windows).
#[cfg(windows)]
use victory_gpu::replay::Options;
#[cfg(not(windows))]
struct Options {
    variants: u32,
    runs: u32,
    dxil: Option<PathBuf>,
    retranslate: bool,
    show: usize,
    diffs: Option<PathBuf>,
    samplers: Vec<String>,
    expect: Option<PathBuf>,
    library: Option<PathBuf>,
    split: u32,
    sensitivity: bool,
    background: bool,
}

#[cfg(windows)]
fn replay_dispatch(dir: &std::path::Path, options: Options) -> Result<()> {
    if let Some(diffs) = &options.diffs {
        std::fs::create_dir_all(diffs)?;
    }
    // On a thread with room for the translator's recursion (the main
    // thread's stack is the linker's 1 MiB).
    let dir = dir.to_owned();
    std::thread::Builder::new()
        .stack_size(64 << 20)
        .spawn(move || victory_gpu::replay::replay_dispatch(&dir, &options).map(|_| ()))?
        .join()
        .map_err(|_| anyhow::anyhow!("the replay panicked"))?
}

#[cfg(not(windows))]
fn replay_dispatch(_: &std::path::Path, _: Options) -> Result<()> {
    anyhow::bail!("replay-dispatch needs D3D12 (Windows)")
}

fn show_config() -> Result<()> {
    let cfg = victory_core::Config::load()?;
    let status = |p: &Option<PathBuf>| match p {
        Some(p) if p.exists() => format!("{}", p.display()),
        Some(p) => format!("{}  (does not exist)", p.display()),
        None => "(not set)".to_owned(),
    };
    println!(
        "config file : {}",
        cfg.source
            .as_ref()
            .map_or("(none)".into(), |p| p.display().to_string())
    );
    println!("game_dir    : {}", status(&cfg.game_dir));
    println!("cache_dir   : {}", cfg.cache_dir.display());
    println!("data_dir    : {}", cfg.data_dir.display());
    for t in &cfg.test_titles {
        println!("test_title  : {}", status(&Some(t.clone())));
    }
    for p in cfg
        .game_dir
        .iter()
        .chain(&cfg.test_titles)
        .chain([&cfg.cache_dir, &cfg.data_dir])
    {
        if let Err(e) = victory_core::ensure_outside_repo(p) {
            println!("POLICY VIOLATION: {e}");
        }
    }
    Ok(())
}

/// Paths given on the command line, or the configured game + test titles.
pub(crate) fn default_paths(paths: Vec<PathBuf>) -> Result<Vec<PathBuf>> {
    if !paths.is_empty() {
        return Ok(paths);
    }
    let cfg = victory_core::Config::load()?;
    let all: Vec<PathBuf> = cfg.game_dir.into_iter().chain(cfg.test_titles).collect();
    anyhow::ensure!(
        !all.is_empty(),
        "no paths given and none configured (see victory.example.toml)"
    );
    Ok(all)
}
