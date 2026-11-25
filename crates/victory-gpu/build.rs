//! Compiles the amplification stage of mesh pipelines (`src/amplify.hlsl`)
//! with the Windows SDK's `dxc` into the build's output: the repository holds
//! only the source (docs/asset-policy.md). Without a compiler the output is
//! empty, no mesh pipeline gets the stage, and mesh draws go the plain way.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    translator_key();
    let source = "src/amplify.hlsl";
    println!("cargo:rerun-if-changed={source}");
    println!("cargo:rerun-if-env-changed=VICTORY_PC_DXC");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR")).join("amplify.dxil");
    let built = dxc().is_some_and(|dxc| {
        Command::new(dxc)
            .args(["-T", "as_6_5", "-E", "main", "-Fo"])
            .arg(&out)
            .arg(source)
            .status()
            .is_ok_and(|s| s.success())
    });
    if !built {
        println!(
            "cargo:warning=no Windows SDK dxc.exe (or VICTORY_PC_DXC): mesh pipelines are built without the amplification stage"
        );
        std::fs::write(&out, []).expect("the build's output is writable");
    }
}

/// What a translated shader depends on, for the shader cache's key
/// (`VICTORY_TRANSLATOR`): the translator's sources and the cache's encoding
/// of its output. Keyed by our whole executable, every build of the GPU
/// crate translated everything again: 100 to 165 s more before the world's
/// first frames, a run in three.
fn translator_key() {
    fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("the translator's sources are readable").flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect(&path, out);
            } else {
                out.push(path);
            }
        }
    }
    let mut files = Vec::new();
    collect(Path::new("../victory-shader/src"), &mut files);
    files.push(PathBuf::from("../victory-shader/Cargo.toml"));
    files.push(PathBuf::from("src/shader_cache.rs"));
    files.sort();
    println!("cargo:rerun-if-changed=../victory-shader/src");
    // Two FNV-1a lanes with different starting points: 128 bits.
    let mut lanes = [0xcbf2_9ce4_8422_2325u64, 0x6c62_272e_07bb_0142];
    for file in &files {
        println!("cargo:rerun-if-changed={}", file.display());
        let bytes = std::fs::read(file).expect("the translator's sources are readable");
        let name = file.to_string_lossy().replace('\\', "/");
        for lane in &mut lanes {
            for &b in name.as_bytes().iter().chain(&bytes) {
                *lane = (*lane ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
    }
    println!("cargo:rustc-env=VICTORY_TRANSLATOR={:016x}{:016x}", lanes[0], lanes[1]);
}

/// `VICTORY_PC_DXC`, else the newest Windows SDK's x64 `dxc.exe` (which signs
/// what it compiles with the `dxil.dll` beside it).
fn dxc() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("VICTORY_PC_DXC") {
        return Some(PathBuf::from(path));
    }
    let kits = PathBuf::from(std::env::var_os("ProgramFiles(x86)")?).join(r"Windows Kits\10\bin");
    let mut found: Vec<PathBuf> = std::fs::read_dir(kits)
        .ok()?
        .filter_map(|e| Some(e.ok()?.path().join(r"x64\dxc.exe")))
        .filter(|dxc| dxc.exists())
        .collect();
    found.sort();
    found.pop()
}
