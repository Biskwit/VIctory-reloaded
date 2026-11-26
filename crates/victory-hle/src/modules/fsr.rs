//! `amd_fidelityfx_loader_xbox.dll`, AMD FidelityFX's entry points: a DLL
//! of the title, which runs as it is. Grand Theft Auto VI's Series S profile
//! upscales with FSR 4 and has no way without it, but FSR 4's shaders carry
//! AMD code the host cannot translate (no console program), so its passes
//! are dropped and its output is never written. Its `ffxDispatch` passes
//! through here: after FSR's own, an upscale gets a bilinear upscale of its
//! colour input into its output on the host
//! (`victory_gpu::list::upscale_stand_in`).
//!
//! No module of the registry, as `bvhbuilder`: [`Interposed`] swaps the
//! import when the title's executable is bound.

use std::sync::atomic::{AtomicUsize, Ordering};

use victory_loader::{ImportResolver, Provenance, Resolution};
use victory_pe::ImportSymbol;

/// The DLL's own `ffxDispatch`, once the title's import of it was seen.
static DLL_DISPATCH: AtomicUsize = AtomicUsize::new(0);

type Dispatch = extern "C" fn(*mut u8, *const u8) -> u32;

/// `ffxDispatchDescUpscale`'s type (`FFX_API_DISPATCH_DESC_TYPE_UPSCALE`).
const UPSCALE: u64 = 0x0001_0001;

/// `ffxDispatch(context, description)`. An upscale description: the type
/// and the next description (16 bytes), the command list at 16, then seven
/// resources of 48 bytes (the resource first) from 24 (colour at 24, output
/// at 312), jitter and motion vector scale, then the render and upscale
/// sizes at 376 and 384.
extern "C" fn dispatch(context: *mut u8, desc: *const u8) -> u32 {
    // SAFETY: set from the DLL's export of that signature before the title runs.
    let dll = unsafe { std::mem::transmute::<usize, Dispatch>(DLL_DISPATCH.load(Ordering::Relaxed)) };
    let done = dll(context, desc);
    // SAFETY: the title's description, of the size its type says.
    #[cfg(windows)]
    if !desc.is_null() && unsafe { desc.cast::<u64>().read_unaligned() } == UPSCALE {
        unsafe {
            let at = |offset: usize| desc.add(offset);
            victory_gpu::list::upscale_stand_in(
                at(16).cast::<*const u8>().read_unaligned(),
                at(24).cast::<*const u8>().read_unaligned(),
                at(376).cast::<[u32; 2]>().read_unaligned(),
                at(312).cast::<*const u8>().read_unaligned(),
                at(384).cast::<[u32; 2]>().read_unaligned(),
            );
        }
    }
    done
}

/// Resolves as the resolver inside, but FidelityFX's `ffxDispatch` goes
/// through this module.
pub struct Interposed<'a>(pub &'a dyn ImportResolver);

impl ImportResolver for Interposed<'_> {
    fn resolve(&self, dll: &str, symbol: &ImportSymbol) -> Resolution {
        match self.0.resolve(dll, symbol) {
            Resolution::Found { address, provenance: Provenance::Title }
                if dll.eq_ignore_ascii_case("amd_fidelityfx_loader_xbox.dll") && symbol.to_string() == "ffxDispatch" =>
            {
                DLL_DISPATCH.store(address, Ordering::Relaxed);
                Resolution::Found { address: dispatch as *const () as usize, provenance: Provenance::Title }
            }
            other => other,
        }
    }
}
