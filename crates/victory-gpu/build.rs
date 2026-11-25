//! Compiles the amplification stage of mesh pipelines (`src/amplify.hlsl`)
//! with the Windows SDK's `dxc` into the build's output: the repository holds
//! only the source (docs/asset-policy.md). Without a compiler the output is
//! empty, no mesh pipeline gets the stage, and mesh draws go the plain way.

use std::path::PathBuf;
use std::process::Command;

fn main() {
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
