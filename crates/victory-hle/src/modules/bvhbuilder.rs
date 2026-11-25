//! `XboxBvhBuilder.dll`, the console's ray-tracing structure builder: a
//! DLL of the title, which runs as it is. Its stages are compute shaders
//! that reach title memory by address, and the host can only give them
//! the memory it knows they will touch (`victory_gpu::structure_builds`).
//! The title says what that is when it calls the builder, in the PC's
//! build descriptions, so the two calls that start dispatches pass
//! through here on their way into the DLL.
//!
//! No module of the registry: a DLL the registry serves is not taken from
//! the title. [`Interposed`] swaps the two imports when the title's
//! executable is bound.

use std::sync::atomic::{AtomicUsize, Ordering};

use victory_loader::{ImportResolver, Provenance, Resolution};
use victory_pe::ImportSymbol;

const BUILD: &str = "?BuildRaytracingAccelerationStructureBatched2X@XboxBvhBuilder@@QEAAXPEAUID3D12GraphicsCommandList6@@IPEBUD3D12XBOX_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_BATCHED_DESC2@@@Z";
const COPY: &str = "?CopyRaytracingAccelerationStructure@XboxBvhBuilder@@QEAAXPEAUID3D12GraphicsCommandList4@@_K1W4D3D12_RAYTRACING_ACCELERATION_STRUCTURE_COPY_MODE@@@Z";
const SIZES: &str = "?GetRaytracingAccelerationStructurePrebuildInfo@XboxBvhBuilder@@QEAAXPEBUD3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS@@PEBUD3D12XBOX_XDXR_STREAM_DESC@@PEAUD3D12_RAYTRACING_ACCELERATION_STRUCTURE_PREBUILD_INFO@@@Z";

/// The DLL's own functions, once the title's imports of them were seen.
static DLL_BUILD: AtomicUsize = AtomicUsize::new(0);
static DLL_COPY: AtomicUsize = AtomicUsize::new(0);
static DLL_SIZES: AtomicUsize = AtomicUsize::new(0);

type Build = extern "system" fn(*const u8, *const u8, u32, *const u8);
type Copy = extern "system" fn(*const u8, *const u8, u64, u64, u32);
type Sizes = extern "system" fn(*const u8, *const u8, *const u8, *mut [u64; 3]);

/// The builder's `BuildRaytracingAccelerationStructureBatched2X`: `count`
/// descriptions of 80 bytes at `descs` (the PC's build description at 0,
/// its inputs at 8, the stream description at 64). The builder is asked
/// for its bounds on each as the title asked before allocating, the host
/// is told, and the DLL builds.
extern "system" fn build(this: *const u8, list: *const u8, count: u32, descs: *const u8) {
    // SAFETY: set from the DLL's export of that signature before the title runs.
    let (dll, sizes) = unsafe {
        (
            std::mem::transmute::<usize, Build>(DLL_BUILD.load(Ordering::Relaxed)),
            std::mem::transmute::<usize, Option<Sizes>>(DLL_SIZES.load(Ordering::Relaxed)),
        )
    };
    #[cfg(windows)]
    let _builds = sizes.filter(|_| !descs.is_null()).map(|sizes| {
        let bounds: Vec<[u64; 3]> = (0..count as usize)
            .map(|i| {
                let mut bound = [0; 3];
                // SAFETY: inside the title's descriptions.
                let desc = unsafe { descs.add(i * 80) };
                // SAFETY: as above; the DLL's function fills the 24 bytes.
                sizes(this, unsafe { desc.add(8) }, unsafe { desc.add(64) }, &mut bound);
                bound
            })
            .collect();
        // SAFETY: the title's descriptions.
        unsafe { victory_gpu::structure_builds(descs, count, &bounds) }
    });
    dll(this, list, count, descs);
}

/// The builder's `CopyRaytracingAccelerationStructure`; it only acts on
/// mode 1, compaction.
extern "system" fn copy(this: *const u8, list: *const u8, dest: u64, source: u64, mode: u32) {
    // SAFETY: as in `build`.
    let dll = unsafe { std::mem::transmute::<usize, Copy>(DLL_COPY.load(Ordering::Relaxed)) };
    #[cfg(windows)]
    let _builds = (mode == 1).then(|| victory_gpu::structure_copy(dest, source));
    dll(this, list, dest, source, mode);
}

/// What the title is bound to instead of the DLL's `symbol` at `address`.
fn interpose(symbol: &str, address: usize) -> usize {
    let (dll, ours) = match symbol {
        BUILD => (&DLL_BUILD, build as *const () as usize),
        COPY => (&DLL_COPY, copy as *const () as usize),
        SIZES => (&DLL_SIZES, address),
        _ => return address,
    };
    dll.store(address, Ordering::Relaxed);
    ours
}

/// Resolves as the resolver inside, but the builder's two calls go
/// through this module.
pub struct Interposed<'a>(pub &'a dyn ImportResolver);

impl ImportResolver for Interposed<'_> {
    fn resolve(&self, dll: &str, symbol: &ImportSymbol) -> Resolution {
        match self.0.resolve(dll, symbol) {
            Resolution::Found { address, provenance: Provenance::Title } if dll.eq_ignore_ascii_case("xboxbvhbuilder.dll") => {
                Resolution::Found { address: interpose(&symbol.to_string(), address), provenance: Provenance::Title }
            }
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Dll;

    impl ImportResolver for Dll {
        fn resolve(&self, _: &str, _: &ImportSymbol) -> Resolution {
            Resolution::Found { address: 0x1234, provenance: Provenance::Title }
        }
    }

    #[test]
    fn only_the_builders_two_calls_are_swapped() {
        let name = |n: &str| ImportSymbol::Name { hint: 0, name: n.into() };
        let found = |dll: &str, n: &str| match Interposed(&Dll).resolve(dll, &name(n)) {
            Resolution::Found { address, .. } => address,
            Resolution::Missing => 0,
        };
        assert_eq!(found("XboxBvhBuilder.dll", BUILD), build as *const () as usize);
        assert_eq!(found("XboxBvhBuilder.dll", COPY), copy as *const () as usize);
        assert_eq!(found("XboxBvhBuilder.dll", SIZES), 0x1234);
        assert_eq!(found("XboxBvhBuilder.dll", "?Set64BitMortonCodes@XboxBvhBuilder@@QEAAX_N@Z"), 0x1234);
        assert_eq!(found("Other.dll", BUILD), 0x1234);
        assert_eq!(DLL_SIZES.load(Ordering::Relaxed), 0x1234);
    }
}
