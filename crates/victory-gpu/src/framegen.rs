//! Optional FidelityFX frame generation using the SDK 2.4 DX12 swap chain.
//! The ABI and swap-chain flow follow PastiiJay's VIctory port (793c56c3).
//! Upscaling stays with the selected provider. Its depth and motion vectors
//! are submitted to AMD on the same graphics command list.

use std::cell::UnsafeCell;
use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::time::Instant;

use windows::Win32::Foundation::{HWND, S_OK};
use windows::Win32::Graphics::Direct3D12::{
    D3D12_HEAP_FLAG_NONE, D3D12_HEAP_PROPERTIES, D3D12_HEAP_TYPE_DEFAULT, D3D12_RESOURCE_STATE_COPY_DEST,
    D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATES, ID3D12CommandQueue, ID3D12Device,
    ID3D12GraphicsCommandList, ID3D12Resource,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_PRESENT, DXGI_SWAP_CHAIN_DESC1, IDXGIFactory2, IDXGISwapChain3, IDXGISwapChain4,
};
use windows::core::{HRESULT, Interface};
use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};

const OK: u32 = 0;
const ERROR: u32 = 1;
const BACKEND_DX12: u64 = 2;
const GET_VERSIONS: u64 = 4;
const OVERRIDE_VERSION: u64 = 5;
const GET_PROVIDER_VERSION: u64 = 6;
const CREATE_FRAMEGEN: u64 = 0x0002_0001;
const CONFIGURE_FRAMEGEN: u64 = 0x0002_0002;
const DISPATCH_FRAMEGEN: u64 = 0x0002_0003;
const PREPARE_FRAMEGEN: u64 = 0x0002_0004;
const FRAMEGEN_VERSION_DESC: u64 = 0x0002_000e;
const FRAMEGEN_VERSION: u32 = (4 << 22) | 2;
const CREATE_SWAP_CHAIN: u64 = 0x0003_0006;
const REGISTER_SWAP_CHAIN_UI: u64 = 0x0003_0002;
const WAIT_FOR_PRESENTS: u64 = 0x0003_0007;
const SWAP_CHAIN_VERSION_DESC: u64 = 0x0003_000b;
const SWAP_CHAIN_VERSION: u32 = 4 << 22;
// The built-in analytical provider in SDK 2.4. Read its ID from the query below.
const FSR3_PROVIDER: u64 = (0xf600_0000 << 32) | (3 << 22) | (1 << 12) | 6;
const PREMULTIPLIED_UI_ALPHA: u32 = 1 << 0;
const TEXTURE_2D: u32 = 2;
const RGBA8_UNORM: u32 = 10;
const RENDER_TARGET_USAGE: u32 = 1;
const PIXEL_COMPUTE_READ: u32 = 0xc;

// AMD requires its callback-driven Present and the context API to be serialized.
static SDK: Mutex<()> = Mutex::new(());
static STOPPED: AtomicBool = AtomicBool::new(false);
static DIR: OnceLock<PathBuf> = OnceLock::new();

#[repr(C)]
struct Header {
    kind: u64,
    next: *mut Header,
}

fn header(kind: u64) -> Header {
    Header {
        kind,
        next: null_mut(),
    }
}

#[repr(C)]
struct Version {
    header: Header,
    version: u32,
}

#[repr(C)]
struct GetVersions {
    header: Header,
    create_kind: u64,
    device: *mut c_void,
    count: *mut u64,
    ids: *mut u64,
    names: *mut *const std::ffi::c_char,
}

#[repr(C)]
struct OverrideVersion {
    header: Header,
    id: u64,
}

#[repr(C)]
struct ProviderVersion {
    header: Header,
    id: u64,
    name: *const std::ffi::c_char,
}

#[repr(C)]
struct CreateSwapChain {
    header: Header,
    swap_chain: *mut *mut c_void,
    hwnd: *mut c_void,
    desc: *const c_void,
    fullscreen: *const c_void,
    factory: *mut c_void,
    queue: *mut c_void,
}

#[repr(C)]
struct Backend {
    header: Header,
    device: *mut c_void,
}

#[repr(C)]
struct CreateFramegen {
    header: Header,
    flags: u32,
    display: [u32; 2],
    max_render: [u32; 2],
    format: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Resource {
    resource: *mut c_void,
    description: [u32; 8],
    state: u32,
}
const _: () = assert!(size_of::<Resource>() == 48);

#[repr(C)]
struct GenerateArgs {
    header: Header,
    command_list: *mut c_void,
    color: Resource,
    outputs: [Resource; 4],
    generated: u32,
}
const _: () = assert!(std::mem::offset_of!(GenerateArgs, generated) == 264);
static GENERATED: AtomicU64 = AtomicU64::new(0);

#[cfg(feature = "diagnostics")]
static FRAME_RATES: AtomicU64 = AtomicU64::new(0);

#[cfg(feature = "diagnostics")]
pub(crate) fn frame_rates() -> Option<(f32, f32)> {
    let rates = FRAME_RATES.load(Ordering::Relaxed);
    let rendered = f32::from_bits((rates >> 32) as u32);
    let presented = f32::from_bits(rates as u32);
    (presented > 0.0).then_some((rendered, presented))
}

const NO_RESOURCE: Resource = Resource {
    resource: null_mut(),
    description: [0; 8],
    state: 0,
};

#[repr(C)]
struct RegisterUi {
    header: Header,
    resource: Resource,
    flags: u32,
}
const _: () = assert!(size_of::<RegisterUi>() == 72);

type Generate = unsafe extern "C" fn(*mut Header, *mut c_void) -> u32;

#[repr(C)]
struct ConfigureFramegen {
    header: Header,
    swap_chain: *mut c_void,
    present_callback: *const c_void,
    present_context: *mut c_void,
    generate_callback: Option<Generate>,
    generate_context: *mut c_void,
    enabled: bool,
    async_workloads: bool,
    hudless: Resource,
    flags: u32,
    only_generated: bool,
    rect: [i32; 4],
    frame: u64,
}
const _: () = assert!(size_of::<ConfigureFramegen>() == 144);

#[repr(C)]
struct Prepare {
    header: Header,
    frame: u64,
    flags: u32,
    command_list: *mut c_void,
    render_size: [u32; 2],
    jitter: [f32; 2],
    motion_scale: [f32; 2],
    frame_ms: f32,
    unused_reset: bool,
    camera: [f32; 4],
    depth: Resource,
    motion: Resource,
}
const _: () = assert!(size_of::<Prepare>() == 184);

type Create = unsafe extern "C" fn(*mut *mut c_void, *mut Header, *const c_void) -> u32;
type Destroy = unsafe extern "C" fn(*mut *mut c_void, *const c_void) -> u32;
type Call = unsafe extern "C" fn(*mut *mut c_void, *mut Header) -> u32;

struct Api {
    create: Create,
    destroy: Destroy,
    configure: Call,
    query: Call,
    dispatch: Call,
}

pub fn requested() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    !STOPPED.load(Ordering::Acquire)
        && *ON.get_or_init(|| std::env::var_os("VICTORY_FRAMEGEN").is_some_and(|v| v == "1"))
}

/// Trial switches, read once:
/// - `VICTORY_FRAMEGEN_FLAGS=<n>`: AMD's debug flags (1: a green bar on the
///   left edge of each generated frame; 16: pacing lines);
/// - `VICTORY_FRAMEGEN_ONLY_GENERATED=1`: present only the generated frames;
/// - `VICTORY_FRAMEGEN_VSYNC=1`: AMD presents both frames on the vblank,
///   without tearing (otherwise with tearing allowed). On by default with
///   `VICTORY_DISPLAY_HZ` (a fixed refresh the title is held to), `0` turns it off;
/// - `VICTORY_FRAMEGEN_ASYNC=1`: generation on AMD's async compute queue;
/// - `VICTORY_FRAMEGEN_DEFER=1`: AMD's preparation at the present of its
///   frame, from copies made on the title's list ([`Slot`]).
#[derive(Clone, Copy)]
struct Trial {
    flags: u32,
    only_generated: bool,
    vsync: bool,
    async_workloads: bool,
    defer: bool,
}

fn trial() -> Trial {
    static TRIAL: OnceLock<Trial> = OnceLock::new();
    *TRIAL.get_or_init(|| {
        let on = |name| std::env::var_os(name).is_some_and(|v| v == "1");
        let fixed_rate = std::env::var_os("VICTORY_DISPLAY_HZ").is_some_and(|v| !v.is_empty());
        let trial = Trial {
            flags: std::env::var("VICTORY_FRAMEGEN_FLAGS").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(0),
            only_generated: on("VICTORY_FRAMEGEN_ONLY_GENERATED"),
            vsync: on("VICTORY_FRAMEGEN_VSYNC")
                || (fixed_rate && std::env::var_os("VICTORY_FRAMEGEN_VSYNC").is_none_or(|v| v != "0")),
            async_workloads: on("VICTORY_FRAMEGEN_ASYNC"),
            defer: on("VICTORY_FRAMEGEN_DEFER"),
        };
        tracing::info!(
            flags = trial.flags,
            only_generated = trial.only_generated,
            vsync = trial.vsync,
            async_workloads = trial.async_workloads,
            defer = trial.defer,
            "frame generation: trial switches"
        );
        trial
    })
}

/// Preparations wait for their present ([`Slot`]): the upscaler's list
/// need not wait for the previous present.
pub(crate) fn deferred() -> bool {
    requested() && trial().defer
}

/// Configure the provider directory before the first swap chain or SDK call.
pub fn set_dir(dir: &Path) {
    let _ = DIR.set(dir.to_path_buf());
}

fn api() -> Option<&'static Api> {
    static API: OnceLock<Option<Api>> = OnceLock::new();
    API.get_or_init(|| match load_api() {
        Ok(api) => Some(api),
        Err(reason) => {
            tracing::warn!(%reason, "frame generation: AMD provider unavailable");
            None
        }
    })
    .as_ref()
}

fn load_api() -> Result<Api, String> {
    let dir = DIR
        .get()
        .cloned()
        .or_else(|| std::env::var_os("VICTORY_FFX_FG_DIR").filter(|v| !v.is_empty()).map(PathBuf::from))
        .or_else(|| std::env::var_os("VICTORY_FIDELITYFX_DIR").filter(|v| !v.is_empty()).map(PathBuf::from))
        .ok_or("set VICTORY_FFX_FG_DIR to the signed FidelityFX frame generation DLL folder")?;
    let load = |path: &Path| {
        let path = std::path::absolute(path).map_err(|e| e.to_string())?;
        let wide: Vec<_> = path.as_os_str().encode_wide().chain([0]).collect();
        // SAFETY: absolute NUL-terminated path. SDK modules stay loaded for playback callbacks.
        let module = unsafe {
            LoadLibraryExW(
                wide.as_ptr(),
                null_mut(),
                LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
            )
        };
        if module.is_null() {
            Err(format!("could not load {}", path.display()))
        } else {
            Ok(module)
        }
    };
    load(&dir.join("amd_fidelityfx_framegeneration_dx12.dll"))?;
    let module = load(&dir.join("amd_fidelityfx_loader_dx12.dll"))?;
    let find = |name: &std::ffi::CStr| {
        // SAFETY: a loaded SDK module and a NUL-terminated export.
        unsafe { GetProcAddress(module, name.as_ptr().cast()) }
            .map(|f| f as *const c_void)
            .ok_or_else(|| format!("FidelityFX loader is missing {}", name.to_string_lossy()))
    };
    // SAFETY: FidelityFX SDK 2.4 C exports.
    unsafe {
        Ok(Api {
            create: std::mem::transmute::<*const c_void, Create>(find(c"ffxCreateContext")?),
            destroy: std::mem::transmute::<*const c_void, Destroy>(find(c"ffxDestroyContext")?),
            configure: std::mem::transmute::<*const c_void, Call>(find(c"ffxConfigure")?),
            query: std::mem::transmute::<*const c_void, Call>(find(c"ffxQuery")?),
            dispatch: std::mem::transmute::<*const c_void, Call>(find(c"ffxDispatch")?),
        })
    }
}

fn analytical_provider(api: &Api, device: &ID3D12Device) -> Option<u64> {
    let mut count = 0;
    let mut query = GetVersions {
        header: header(GET_VERSIONS),
        create_kind: CREATE_FRAMEGEN,
        device: device.as_raw(),
        count: &raw mut count,
        ids: null_mut(),
        names: null_mut(),
    };
    // SAFETY: SDK version enumeration uses a null context and writes count.
    if unsafe { (api.query)(null_mut(), &raw mut query.header) } != OK || count == 0 {
        return None;
    }
    let mut ids = vec![0; usize::try_from(count).ok()?];
    query.ids = ids.as_mut_ptr();
    // SAFETY: the SDK sees the capacity in count and writes within the ID array.
    if unsafe { (api.query)(null_mut(), &raw mut query.header) } != OK {
        return None;
    }
    ids.into_iter()
        .take(usize::try_from(count).ok()?)
        .find(|&id| id == FSR3_PROVIDER)
}

struct Context {
    handle: UnsafeCell<*mut c_void>,
    can_generate: AtomicBool,
    create: Box<CreateFramegen>,
    _backend: Box<Backend>,
    _version: Box<Version>,
    _provider: Box<OverrideVersion>,
    _device: ID3D12Device,
}

// SAFETY: the descriptors and device stay alive and immutable after creation.
// Preparation and configuration are serialized by SDK. The SDK owns its
// presentation callback pipeline. Resize drains it before dropping the context.
unsafe impl Send for Context {}
// SAFETY: as above. Recorded lists keep their own Arc through GPU completion.
unsafe impl Sync for Context {}

impl Context {
    /// Requires the SDK guard and no live recordings or swap-chain callbacks.
    fn destroy(&mut self, api: &Api) -> bool {
        let handle = self.handle.get_mut();
        // SAFETY: this is the sole owner and the caller has drained its work.
        let result = unsafe { (api.destroy)(handle, null()) };
        if result != OK {
            tracing::warn!(result, "frame generation: AMD could not destroy the generation context");
            return false;
        }
        *handle = null_mut();
        true
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        if !self.handle.get_mut().is_null()
            && let Some(api) = api()
        {
            let _sdk = SDK.lock().unwrap_or_else(|e| e.into_inner());
            self.destroy(api);
        }
    }
}

const RECORDED: u8 = 0;
const SUBMITTED: u8 = 1;
const DISCARDED: u8 = 2;

struct PreparedFrame {
    frame: u64,
    title_list: usize,
    token: Option<u64>,
    enabled: bool,
    submission: Arc<AtomicU8>,
}

impl PreparedFrame {
    fn matches_present(&self, frame: u64, token: Option<u64>) -> bool {
        self.frame == frame && token.is_some() && self.token == token && self.submitted()
    }

    fn submitted(&self) -> bool {
        self.submission.load(Ordering::Acquire) == SUBMITTED
    }
}

/// A host list owns the SDK resources it recorded through GPU completion.
/// Resetting an unsent list frees its frame slot without touching the SDK.
pub(crate) struct Preparation {
    _context: Option<Arc<Context>>,
    recording: Option<Recording>,
    /// The deferred slot's copies the list writes ([`Slot`]).
    _kept: Vec<ID3D12Resource>,
}

impl Preparation {
    pub(crate) fn submitted(&self) {
        if let Some(recording) = &self.recording {
            recording.submitted();
        }
    }
}

struct Recording(Arc<AtomicU8>);

impl Recording {
    fn submitted(&self) {
        self.0.store(SUBMITTED, Ordering::Release);
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        let _ = self.0.compare_exchange(RECORDED, DISCARDED, Ordering::AcqRel, Ordering::Acquire);
    }
}

/// `VICTORY_FRAMEGEN_DEFER=1`. A title held at its frame cap records frame
/// N+1's upscale while it waits to present frame N, so a preparation made
/// on the upscaler's list can belong to the frame after the next present.
/// AMD's analytical provider keeps one set of prepared buffers (it flips
/// them at each prepare) and resets a generation whose frame is not the
/// last one configured, so its configure and prepare have to come right
/// before their own present. The depth and motion vectors are copied on
/// the title's list into one of two slots; the slot is bound to the token
/// the title submits that list with, and the present with that token
/// configures and prepares from the copies on the host's list.
struct Slot {
    /// Depth and motion copies, in COPY_DEST between uses.
    textures: Option<[ID3D12Resource; 2]>,
    pending: Option<Pending>,
}

/// One frame's copied inputs, waiting for their present.
struct Pending {
    /// Order of recording: the oldest unbound one takes a submission.
    seq: u64,
    title_list: usize,
    token: Option<u64>,
    submission: Arc<AtomicU8>,
    depth_desc: [u32; 8],
    motion_desc: [u32; 8],
    max_render: [u32; 2],
    render_size: [u32; 2],
    upscale_flags: u32,
    jitter: [f32; 2],
    motion_scale: [f32; 2],
    frame_ms: f32,
    reset: bool,
    camera: [f32; 4],
}

const SLOTS: usize = 2;

/// Frees slots whose list was reset unsent.
fn drop_discarded(slots: &mut [Slot]) {
    for slot in slots {
        if slot.pending.as_ref().is_some_and(|p| p.submission.load(Ordering::Acquire) == DISCARDED) {
            slot.pending = None;
        }
    }
}

/// Binds the oldest unbound slot recorded on one of `lists` to `token`.
fn bind_slot(slots: &mut [Slot], lists: &[usize], token: u64) {
    let oldest = slots
        .iter_mut()
        .filter_map(|slot| slot.pending.as_mut())
        .filter(|p| {
            p.token.is_none()
                && p.submission.load(Ordering::Acquire) != DISCARDED
                && lists.contains(&p.title_list)
        })
        .min_by_key(|p| p.seq);
    if let Some(pending) = oldest {
        pending.token = Some(token);
    }
}

/// The slot for the present with `token`, taken out, and any slot bound to
/// an earlier token freed (its frame was not presented). The count of
/// those freed comes back too.
fn take_slot(slots: &mut [Slot], token: u64) -> (Option<(usize, Pending)>, u64) {
    let mut stale = 0;
    for slot in slots.iter_mut() {
        if slot.pending.as_ref().is_some_and(|p| p.token.is_some_and(|t| t < token)) {
            slot.pending = None;
            stale += 1;
        }
    }
    let index = slots
        .iter()
        .position(|slot| slot.pending.as_ref().is_some_and(|p| p.token == Some(token)));
    (index.and_then(|i| slots[i].pending.take().map(|p| (i, p))), stale)
}

/// FidelityFX's read state of a preparation input, as the host list's.
fn read_state(ffx: u32) -> D3D12_RESOURCE_STATES {
    // FFX pixel and compute read (0xc) or compute read (0x4).
    D3D12_RESOURCE_STATES(if ffx == 0xc { 0xc0 } else { 0x40 })
}

/// A default-heap texture of `source`'s description, in COPY_DEST.
fn copy_texture(source: &ID3D12Resource) -> Option<ID3D12Resource> {
    let device = crate::external::host_device()?;
    // SAFETY: the description of a live resource, used for a new one.
    unsafe {
        let desc = source.GetDesc();
        let heap = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() };
        let mut texture: Option<ID3D12Resource> = None;
        device
            .CreateCommittedResource(
                &heap,
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATE_COPY_DEST,
                None,
                &mut texture,
            )
            .ok()?;
        texture
    }
}

fn same_texture(a: &ID3D12Resource, b: &ID3D12Resource) -> bool {
    // SAFETY: plain queries of live resources.
    let (a, b) = unsafe { (a.GetDesc(), b.GetDesc()) };
    (a.Width, a.Height, a.Format, a.MipLevels, a.DepthOrArraySize, a.Flags)
        == (b.Width, b.Height, b.Format, b.MipLevels, b.DepthOrArraySize, b.Flags)
}

#[derive(Default)]
struct State {
    swap_context: usize,
    swap_chain: usize,
    display: [u32; 2],
    format: u32,
    ui: Option<ID3D12Resource>,
    provider: u64,
    context: Option<Arc<Context>>,
    refused_layout: Option<([u32; 2], u32)>,
    prepared: Option<PreparedFrame>,
    /// Deferred preparation ([`Slot`]): the slots, the next recording's
    /// order, AMD's frame number of the last preparation, and the token
    /// and enable of the preparation made for the coming present.
    slots: Vec<Slot>,
    seq: u64,
    frame_id: u64,
    ready: Option<(u64, bool)>,
    last_present: Option<u64>,
    real: u64,
    sample: Option<(Instant, u64, u32, u64)>,
    #[cfg(feature = "diagnostics")]
    cadence: Cadence,
}

#[cfg(feature = "diagnostics")]
#[derive(Default)]
struct Cadence {
    prepare_seen: u64,
    no_swap_chain: u64,
    slot_occupied: u64,
    wrong_frame: u64,
    first_frame_gap: Option<(Option<u64>, u64)>,
    invalid_extent: u64,
    context_unavailable: u64,
    layout_changed: u64,
    configure_failed: u64,
    prepare_failed: u64,
    prepared: u64,
    present_no_prepare: u64,
    present_wrong_frame: u64,
    present_wrong_token: u64,
    present_before_submission: u64,
    present_reset: u64,
    /// Deferred slots freed unpresented (a later token was presented).
    stale: u64,
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(|| Mutex::new(State::default()));

fn surface_format(dxgi: i32) -> Option<u32> {
    Some(match dxgi {
        28 => 10,
        87 => 14,
        24 => 17,
        10 => 4,
        _ => return None,
    })
}

pub(crate) fn swap_chain(
    hwnd: HWND,
    desc: &DXGI_SWAP_CHAIN_DESC1,
    factory: &IDXGIFactory2,
    queue: &ID3D12CommandQueue,
    device: &ID3D12Device,
    frame: u64,
) -> Option<IDXGISwapChain3> {
    let api = api()?;
    let format = surface_format(desc.Format.0)?;
    let _sdk = SDK.lock().unwrap_or_else(|e| e.into_inner());
    if !requested() {
        return None;
    }
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let Some(provider) = analytical_provider(api, device) else {
        tracing::warn!("frame generation: AMD analytical provider 3.1.6 is unavailable");
        return None;
    };
    let mut context = null_mut();
    let mut out = null_mut();
    let mut version = Version {
        header: header(SWAP_CHAIN_VERSION_DESC),
        version: SWAP_CHAIN_VERSION,
    };
    let mut create = CreateSwapChain {
        header: Header {
            kind: CREATE_SWAP_CHAIN,
            next: &raw mut version.header,
        },
        swap_chain: &raw mut out,
        hwnd: hwnd.0,
        desc: std::ptr::from_ref(desc).cast(),
        fullscreen: null(),
        factory: factory.as_raw(),
        queue: queue.as_raw(),
    };
    // SAFETY: the DXGI objects and the create descriptors remain live for the call.
    let result = unsafe { (api.create)(&mut context, &raw mut create.header, null()) };
    if result != OK || context.is_null() || out.is_null() {
        tracing::warn!(result, "frame generation: AMD swap chain creation failed");
        if !context.is_null() {
            // SAFETY: a context returned by the SDK with a failed or partial create.
            unsafe { (api.destroy)(&mut context, null()) };
        }
        return None;
    }
    // SAFETY: the SDK's replacement swap chain, which returned one COM reference.
    let swap = unsafe { IDXGISwapChain4::from_raw(out) };
    let Ok(swap_chain) = swap.cast::<IDXGISwapChain3>() else {
        // SAFETY: the new swap chain has no submitted presents.
        unsafe { (api.destroy)(&mut context, null()) };
        return None;
    };
    state.swap_context = context as usize;
    state.swap_chain = out as usize;
    state.display = [desc.Width, desc.Height];
    state.format = format;
    state.provider = provider;
    // The swap chain is created to present this title frame.
    state.last_present = frame.checked_sub(1);
    tracing::info!(
        width = desc.Width,
        height = desc.Height,
        "frame generation: AMD swap chain active"
    );
    Some(swap_chain)
}

/// Depth, motion and camera data from the title's successful upscaler call.
pub struct Inputs<'a> {
    pub depth: &'a ID3D12Resource,
    pub motion: &'a ID3D12Resource,
    pub depth_desc: [u32; 8],
    pub motion_desc: [u32; 8],
    pub depth_state: u32,
    pub motion_state: u32,
    pub max_render: [u32; 2],
    pub render_size: [u32; 2],
    pub upscale_flags: u32,
    pub jitter: [f32; 2],
    pub motion_scale: [f32; 2],
    pub frame_ms: f32,
    pub reset: bool,
    pub camera: [f32; 4],
}

fn make_context(api: &Api, state: &State, max_render: [u32; 2], upscale_flags: u32) -> Option<Arc<Context>> {
    let device = crate::external::host_device()?;
    if state.format == 0 || state.display.contains(&0) || max_render.contains(&0) {
        return None;
    }
    let mut version = Box::new(Version {
        header: header(FRAMEGEN_VERSION_DESC),
        version: FRAMEGEN_VERSION,
    });
    let mut provider = Box::new(OverrideVersion {
        header: Header {
            kind: OVERRIDE_VERSION,
            next: &raw mut version.header,
        },
        id: state.provider,
    });
    let mut backend = Box::new(Backend {
        header: Header {
            kind: BACKEND_DX12,
            next: &raw mut provider.header,
        },
        device: device.as_raw(),
    });
    let mut create = Box::new(CreateFramegen {
        header: Header {
            kind: CREATE_FRAMEGEN,
            next: &raw mut backend.header,
        },
        // Keep the depth/MV flags. The swap chain's target is SDR.
        flags: upscale_flags & 0b1_1110,
        display: state.display,
        max_render,
        format: state.format,
    });
    let mut handle = null_mut();
    // SAFETY: the boxed descriptors and the device outlive the context.
    let result = unsafe { (api.create)(&mut handle, &raw mut create.header, null()) };
    if result != OK || handle.is_null() {
        if !handle.is_null() {
            // SAFETY: release any partial context that the SDK returned.
            unsafe { (api.destroy)(&mut handle, null()) };
        }
        static REPORTED: std::sync::Once = std::sync::Once::new();
        REPORTED
            .call_once(|| tracing::warn!(result, "frame generation: AMD context creation failed"));
        return None;
    }
    let mut selected = ProviderVersion {
        header: header(GET_PROVIDER_VERSION),
        id: 0,
        name: null(),
    };
    // SAFETY: a live context and the SDK's provider query descriptor.
    let result = unsafe { (api.query)(&mut handle, &raw mut selected.header) };
    if result != OK || selected.id != state.provider {
        tracing::warn!(
            result,
            provider = selected.id,
            "frame generation: AMD selected an unexpected provider"
        );
        // SAFETY: context creation succeeded and its GPU work has not been recorded.
        unsafe { (api.destroy)(&mut handle, null()) };
        return None;
    }
    tracing::info!(display = ?state.display, render = ?max_render, "frame generation: AMD 3.1.6 context created");
    Some(Arc::new(Context {
        handle: UnsafeCell::new(handle),
        can_generate: AtomicBool::new(false),
        create,
        _backend: backend,
        _version: version,
        _provider: provider,
        _device: device,
    }))
}

pub(crate) fn prepare(
    list: &ID3D12GraphicsCommandList,
    title_list: crate::object::This,
    input: Inputs<'_>,
) -> Option<Preparation> {
    if !requested() {
        return None;
    }
    let api = api()?;
    // The title increments this number before it queues the host Present.
    let frame = crate::queue::presented() + 1;
    // Drop retired contexts after the SDK guard, since Context::drop takes it.
    let _retired;
    let _sdk = SDK.lock().unwrap_or_else(|e| e.into_inner());
    // An upscaler call could have been waiting on this guard when a
    // presenter started retiring its proxy.
    if !requested() {
        return None;
    }
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    #[cfg(feature = "diagnostics")]
    {
        state.cadence.prepare_seen += 1;
    }
    // Keep the analytical provider's shared prep buffers and its swap-chain
    // configuration paired with the next actual Present.
    if state.swap_context == 0 {
        #[cfg(feature = "diagnostics")]
        {
            state.cadence.no_swap_chain += 1;
        }
        return None;
    }
    if trial().defer {
        return copy_for_present(list, title_list, &input, &mut state);
    }
    if state.prepared.as_ref().is_some_and(|p| p.submission.load(Ordering::Acquire) == DISCARDED) {
        state.prepared = None;
    }
    if state.prepared.is_some() {
        #[cfg(feature = "diagnostics")]
        {
            state.cadence.slot_occupied += 1;
        }
        return None;
    }
    if state.last_present.and_then(|last| last.checked_add(1)) != Some(frame) {
        #[cfg(feature = "diagnostics")]
        {
            let last_present = state.last_present;
            state.cadence.wrong_frame += 1;
            state
                .cadence
                .first_frame_gap
                .get_or_insert((last_present, frame));
        }
        return None;
    }
    if input.render_size[0] > state.display[0] || input.render_size[1] > state.display[1] {
        #[cfg(feature = "diagnostics")]
        {
            state.cadence.invalid_extent += 1;
        }
        return None;
    }
    let layout = (input.max_render, input.upscale_flags & 0b1_1110);
    if state.context.as_ref().is_some_and(|context| {
        (context.create.max_render, context.create.flags) != layout
            || context.create.display != state.display
            || context.create.format != state.format
    }) {
        #[cfg(feature = "diagnostics")]
        {
            state.cadence.layout_changed += 1;
        }
        if !disable_and_drain(api, &state, frame) {
            return None;
        }
        _retired = state.context.take();
        state.refused_layout = None;
    }
    if state.context.is_none() && state.refused_layout != Some(layout) {
        state.context = make_context(api, &state, input.max_render, input.upscale_flags);
        if state.context.is_none() {
            state.refused_layout = Some(layout);
        }
    }
    let Some(context) = state.context.clone() else {
        #[cfg(feature = "diagnostics")]
        {
            state.cadence.context_unavailable += 1;
        }
        return None;
    };
    let enabled = !input.reset;
    if !configure(api, &state, frame, enabled) {
        #[cfg(feature = "diagnostics")]
        {
            state.cadence.configure_failed += 1;
        }
        return None;
    }
    // SDK 2.4's analytical provider accepts this description without a
    // camera view matrix. Its legacy reset field is ignored, so present
    // real frames only when the upscaler signals a cut.
    let mut prepare = Prepare {
        header: header(PREPARE_FRAMEGEN),
        frame,
        flags: 0,
        command_list: list.as_raw(),
        render_size: input.render_size,
        jitter: input.jitter,
        motion_scale: input.motion_scale,
        frame_ms: input.frame_ms,
        unused_reset: false,
        camera: input.camera,
        depth: Resource {
            resource: input.depth.as_raw(),
            description: input.depth_desc,
            state: input.depth_state,
        },
        motion: Resource {
            resource: input.motion.as_raw(),
            description: input.motion_desc,
            state: input.motion_state,
        },
    };
    // SAFETY: the upscaler's list is still open and holds both input resources.
    let result = unsafe { (api.dispatch)(context.handle.get(), &raw mut prepare.header) };
    if result != OK {
        #[cfg(feature = "diagnostics")]
        {
            state.cadence.prepare_failed += 1;
        }
        static REPORTED: std::sync::Once = std::sync::Once::new();
        REPORTED.call_once(|| {
            tracing::warn!(result, "frame generation: AMD prepare refused the frame")
        });
        // The SDK may already have recorded work before a failed return.
        return Some(Preparation { _context: Some(context), recording: None, _kept: Vec::new() });
    }
    let submission = Arc::new(AtomicU8::new(RECORDED));
    state.prepared = Some(PreparedFrame {
        frame,
        title_list: title_list as usize,
        token: None,
        enabled,
        submission: submission.clone(),
    });
    #[cfg(feature = "diagnostics")]
    {
        state.cadence.prepared += 1;
    }
    static FIRST: std::sync::Once = std::sync::Once::new();
    FIRST.call_once(|| tracing::info!(frame, render = ?input.render_size, "frame generation: first AMD prepare recorded"));
    Some(Preparation { _context: Some(context), recording: Some(Recording(submission)), _kept: Vec::new() })
}

/// Deferred preparation ([`Slot`]): copies the frame's depth and motion
/// vectors into a free slot on the upscaler's list.
fn copy_for_present(
    list: &ID3D12GraphicsCommandList,
    title_list: crate::object::This,
    input: &Inputs<'_>,
    state: &mut State,
) -> Option<Preparation> {
    drop_discarded(&mut state.slots);
    while state.slots.len() < SLOTS {
        state.slots.push(Slot { textures: None, pending: None });
    }
    let Some(index) = state.slots.iter().position(|slot| slot.pending.is_none()) else {
        #[cfg(feature = "diagnostics")]
        {
            state.cadence.slot_occupied += 1;
        }
        return None;
    };
    let slot = &mut state.slots[index];
    if !slot.textures.as_ref().is_some_and(|[depth, motion]| {
        same_texture(depth, input.depth) && same_texture(motion, input.motion)
    }) {
        slot.textures = Some([copy_texture(input.depth)?, copy_texture(input.motion)?]);
    }
    let [depth, motion] = slot.textures.clone()?;
    let (depth_state, motion_state) = (read_state(input.depth_state), read_state(input.motion_state));
    use crate::host::transition;
    // SAFETY: the upscaler's list is open and holds both inputs in their read states.
    unsafe {
        list.ResourceBarrier(&[
            transition(input.depth, depth_state, D3D12_RESOURCE_STATE_COPY_SOURCE),
            transition(input.motion, motion_state, D3D12_RESOURCE_STATE_COPY_SOURCE),
        ]);
        list.CopyResource(&depth, input.depth);
        list.CopyResource(&motion, input.motion);
        list.ResourceBarrier(&[
            transition(input.depth, D3D12_RESOURCE_STATE_COPY_SOURCE, depth_state),
            transition(input.motion, D3D12_RESOURCE_STATE_COPY_SOURCE, motion_state),
        ]);
    }
    state.seq += 1;
    let submission = Arc::new(AtomicU8::new(RECORDED));
    state.slots[index].pending = Some(Pending {
        seq: state.seq,
        title_list: title_list as usize,
        token: None,
        submission: submission.clone(),
        depth_desc: input.depth_desc,
        motion_desc: input.motion_desc,
        max_render: input.max_render,
        render_size: input.render_size,
        upscale_flags: input.upscale_flags,
        jitter: input.jitter,
        motion_scale: input.motion_scale,
        frame_ms: input.frame_ms,
        reset: input.reset,
        camera: input.camera,
    });
    static FIRST: std::sync::Once = std::sync::Once::new();
    FIRST.call_once(|| tracing::info!(render = ?input.render_size, "frame generation: first input copied for its present (VICTORY_FRAMEGEN_DEFER)"));
    Some(Preparation { _context: None, recording: Some(Recording(submission)), _kept: vec![depth, motion] })
}

/// Deferred preparation ([`Slot`]): on the host's list of the present with
/// `token`, configures AMD for that frame and prepares it from the slot's
/// copies. Returns the copies, for the list to hold until it has run.
pub(crate) fn present_prepare(list: &ID3D12GraphicsCommandList, token: Option<u64>) -> Option<[ID3D12Resource; 2]> {
    if !deferred() {
        return None;
    }
    let api = api()?;
    // Drop retired contexts after the SDK guard, since Context::drop takes it.
    let _retired;
    let _sdk = SDK.lock().unwrap_or_else(|e| e.into_inner());
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    state.ready = None;
    if state.swap_context == 0 {
        return None;
    }
    drop_discarded(&mut state.slots);
    let (taken, _stale) = token.map_or((None, 0), |token| take_slot(&mut state.slots, token));
    #[cfg(feature = "diagnostics")]
    {
        state.cadence.stale += _stale;
    }
    let Some((index, pending)) = taken else {
        #[cfg(feature = "diagnostics")]
        {
            state.cadence.present_no_prepare += 1;
        }
        return None;
    };
    if pending.submission.load(Ordering::Acquire) != SUBMITTED {
        #[cfg(feature = "diagnostics")]
        {
            state.cadence.present_before_submission += 1;
        }
        return None;
    }
    let [depth, motion] = state.slots[index].textures.clone()?;
    if pending.render_size[0] > state.display[0] || pending.render_size[1] > state.display[1] {
        #[cfg(feature = "diagnostics")]
        {
            state.cadence.invalid_extent += 1;
        }
        return None;
    }
    let frame = state.frame_id + 1;
    let layout = (pending.max_render, pending.upscale_flags & 0b1_1110);
    if state.context.as_ref().is_some_and(|context| {
        (context.create.max_render, context.create.flags) != layout
            || context.create.display != state.display
            || context.create.format != state.format
    }) {
        #[cfg(feature = "diagnostics")]
        {
            state.cadence.layout_changed += 1;
        }
        if !disable_and_drain(api, &state, state.frame_id) {
            return None;
        }
        _retired = state.context.take();
        state.refused_layout = None;
    }
    if state.context.is_none() && state.refused_layout != Some(layout) {
        state.context = make_context(api, &state, pending.max_render, pending.upscale_flags);
        if state.context.is_none() {
            state.refused_layout = Some(layout);
        }
    }
    let Some(context) = state.context.clone() else {
        #[cfg(feature = "diagnostics")]
        {
            state.cadence.context_unavailable += 1;
        }
        return None;
    };
    let enabled = !pending.reset;
    if !configure(api, &state, frame, enabled) {
        #[cfg(feature = "diagnostics")]
        {
            state.cadence.configure_failed += 1;
        }
        return None;
    }
    const FFX_COMPUTE_READ: u32 = 0x4;
    let shader_read = read_state(FFX_COMPUTE_READ);
    let mut prepare = Prepare {
        header: header(PREPARE_FRAMEGEN),
        frame,
        flags: 0,
        command_list: list.as_raw(),
        render_size: pending.render_size,
        jitter: pending.jitter,
        motion_scale: pending.motion_scale,
        frame_ms: pending.frame_ms,
        unused_reset: false,
        camera: pending.camera,
        depth: Resource { resource: depth.as_raw(), description: pending.depth_desc, state: FFX_COMPUTE_READ },
        motion: Resource { resource: motion.as_raw(), description: pending.motion_desc, state: FFX_COMPUTE_READ },
    };
    use crate::host::transition;
    // SAFETY: the present's open host list; the copies stay alive through
    // it (returned to the caller, which keeps them on the list).
    let result = unsafe {
        list.ResourceBarrier(&[
            transition(&depth, D3D12_RESOURCE_STATE_COPY_DEST, shader_read),
            transition(&motion, D3D12_RESOURCE_STATE_COPY_DEST, shader_read),
        ]);
        let result = (api.dispatch)(context.handle.get(), &raw mut prepare.header);
        list.ResourceBarrier(&[
            transition(&depth, shader_read, D3D12_RESOURCE_STATE_COPY_DEST),
            transition(&motion, shader_read, D3D12_RESOURCE_STATE_COPY_DEST),
        ]);
        result
    };
    if result != OK {
        #[cfg(feature = "diagnostics")]
        {
            state.cadence.prepare_failed += 1;
        }
        static REPORTED: std::sync::Once = std::sync::Once::new();
        REPORTED.call_once(|| tracing::warn!(result, "frame generation: AMD prepare refused the frame"));
        return Some([depth, motion]);
    }
    state.frame_id = frame;
    state.ready = token.map(|token| (token, enabled));
    #[cfg(feature = "diagnostics")]
    {
        state.cadence.prepared += 1;
    }
    static FIRST: std::sync::Once = std::sync::Once::new();
    FIRST.call_once(|| tracing::info!(frame, render = ?pending.render_size, "frame generation: first AMD prepare at its present (VICTORY_FRAMEGEN_DEFER)"));
    Some([depth, motion])
}

/// Bind the recorded upscaler input to the token supplied by the title on
/// submission. Reusing the same list must not change an already bound token.
pub(crate) fn bind_submission(lists: &[crate::object::This], token: u64) {
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    if trial().defer {
        let lists: Vec<usize> = lists.iter().map(|&list| list as usize).collect();
        bind_slot(&mut state.slots, &lists, token);
        return;
    }
    if let Some(prepared) = state.prepared.as_mut()
        && prepared.token.is_none()
        && prepared.submission.load(Ordering::Acquire) != DISCARDED
        && lists
            .iter()
            .any(|&list| list as usize == prepared.title_list)
    {
        prepared.token = Some(token);
    }
}

unsafe extern "C" fn generate(params: *mut Header, context: *mut c_void) -> u32 {
    let Some(api) = api() else { return ERROR };
    // SAFETY: the SDK passes a live generation descriptor to this callback.
    if !unsafe { params.as_ref() }.is_some_and(|header| header.kind == DISPATCH_FRAMEGEN) {
        return ERROR;
    }
    // SAFETY: the configured Context stays live until the proxy is drained.
    let Some(context) = (unsafe { context.cast::<Context>().as_ref() }) else { return ERROR };
    if !context.can_generate.load(Ordering::Acquire) {
        // SAFETY: the callback receives the SDK's generation descriptor.
        unsafe { (*params.cast::<GenerateArgs>()).generated = 0 };
        return OK;
    }
    // SAFETY: the SDK calls back from Present, which owns the SDK guard.
    let result = unsafe { (api.dispatch)(context.handle.get(), params) };
    if result == OK {
        // SAFETY: the checked descriptor type includes the generated count.
        let count = unsafe { (*params.cast::<GenerateArgs>()).generated };
        GENERATED.fetch_add(u64::from(count), Ordering::Relaxed);
        static FIRST: std::sync::Once = std::sync::Once::new();
        FIRST.call_once(|| {
            tracing::info!("frame generation: first AMD generation callback succeeded")
        });
    } else {
        static REPORTED: std::sync::Once = std::sync::Once::new();
        REPORTED.call_once(|| {
            tracing::warn!(result, "frame generation: AMD generation callback failed")
        });
    }
    result
}

fn configuration(state: &State, context: *mut c_void, frame: u64, enabled: bool) -> ConfigureFramegen {
    ConfigureFramegen {
        header: header(CONFIGURE_FRAMEGEN),
        swap_chain: state.swap_chain as *mut c_void,
        present_callback: null(),
        present_context: null_mut(),
        generate_callback: enabled.then_some(generate as Generate),
        generate_context: if enabled { context } else { null_mut() },
        enabled,
        async_workloads: trial().async_workloads,
        hudless: NO_RESOURCE,
        flags: trial().flags,
        only_generated: trial().only_generated,
        rect: [0, 0, state.display[0] as i32, state.display[1] as i32],
        frame,
    }
}

fn configure(api: &Api, state: &State, frame: u64, enabled: bool) -> bool {
    let Some(context) = &state.context else {
        return false;
    };
    let mut desc = configuration(state, Arc::as_ptr(context).cast_mut().cast(), frame, enabled);
    // SAFETY: the presenter owns the live swap chain and the context survives its callback.
    let result = unsafe { (api.configure)(context.handle.get(), &raw mut desc.header) };
    if result != OK {
        static REPORTED: std::sync::Once = std::sync::Once::new();
        REPORTED
            .call_once(|| tracing::warn!(result, "frame generation: AMD configuration refused"));
    }
    result == OK
}

fn wait_for_presents(api: &Api, state: &State) -> bool {
    let mut context = state.swap_context as *mut c_void;
    // SAFETY: the SDK guard protects the live swap chain and its owner.
    let result = unsafe { (api.dispatch)(&mut context, &mut header(WAIT_FOR_PRESENTS)) };
    if result != OK {
        tracing::warn!(result, "frame generation: AMD could not drain the swap chain");
    }
    result == OK
}

fn disable_and_drain(api: &Api, state: &State, frame: u64) -> bool {
    let disabled = state.context.as_ref().is_none_or(|context| {
        context.can_generate.store(false, Ordering::Release);
        configure(api, state, frame, false)
    });
    let drained = wait_for_presents(api, state);
    disabled && drained
}

/// Set the optional premultiplied image the proxy composites. The presenter
/// supplies alternating images so the proxy can read the previous one.
fn register_ui(api: &Api, state: &mut State, ui: Option<&ID3D12Resource>) -> bool {
    if ui.is_none() && state.ui.is_none() {
        return true;
    }
    let [width, height] = state.display;
    let mut setup = RegisterUi {
        header: header(REGISTER_SWAP_CHAIN_UI),
        resource: ui.map_or(NO_RESOURCE, |texture| Resource {
            resource: texture.as_raw(),
            description: [
                TEXTURE_2D,
                RGBA8_UNORM,
                width,
                height,
                1,
                1,
                0,
                RENDER_TARGET_USAGE,
            ],
            state: PIXEL_COMPUTE_READ,
        }),
        flags: ui.map_or(0, |_| PREMULTIPLIED_UI_ALPHA),
    };
    let mut context = state.swap_context as *mut c_void;
    // SAFETY: the swap-chain context is live and the panel owns this image.
    let result = unsafe { (api.configure)(&mut context, &raw mut setup.header) };
    if result != OK {
        static REPORTED: std::sync::Once = std::sync::Once::new();
        REPORTED.call_once(|| {
            tracing::warn!(
                result,
                "frame generation: AMD could not register the UI plane"
            )
        });
        return false;
    }
    if ui.is_some() {
        static FIRST: std::sync::Once = std::sync::Once::new();
        FIRST.call_once(|| {
            tracing::info!(
                "frame generation: UI plane registered for separate composition"
            )
        });
    }
    state.ui = ui.cloned();
    true
}

pub(crate) fn set_ui(ui: Option<&ID3D12Resource>) -> bool {
    let _sdk = SDK.lock().unwrap_or_else(|e| e.into_inner());
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    api().is_some_and(|api| register_ui(api, &mut state, ui))
}

pub(crate) fn present(
    swap_chain: &IDXGISwapChain3,
    frame: u64,
    token: Option<u64>,
    allow_generation: bool,
) -> HRESULT {
    let _sdk = SDK.lock().unwrap_or_else(|e| e.into_inner());
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    // Deferred: `present_prepare` configured this frame on the present's
    // list (or nothing was prepared for it). AMD's frame number is ours.
    let (enabled, disable, frame) = if trial().defer {
        let ready = state.ready.take().filter(|&(ready, _)| Some(ready) == token);
        #[cfg(feature = "diagnostics")]
        if ready.is_some_and(|(_, enabled)| !enabled) {
            state.cadence.present_reset += 1;
        }
        let prepared = ready.is_some_and(|(_, enabled)| enabled);
        (prepared && allow_generation, !(prepared && allow_generation), state.frame_id)
    } else {
        let (enabled, disable) = matched_present(&mut state, frame, token, allow_generation);
        (enabled, disable, frame)
    };
    if disable && state.context.is_some()
        && let Some(api) = api()
    {
        configure(api, &state, frame, false);
    }
    if let Some(context) = &state.context {
        context.can_generate.store(enabled, Ordering::Release);
    }
    // The SDK guard serializes the callback with other context calls. The
    // state lock does not have to be held while the proxy paces presentation.
    drop(state);
    // SAFETY: the caller holds the presenter and has submitted the back buffer.
    let status = unsafe { swap_chain.Present(u32::from(trial().vsync), DXGI_PRESENT(0)) };
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(context) = &state.context {
        context.can_generate.store(false, Ordering::Release);
    }
    state.last_present = Some(frame);
    if status == S_OK {
        // SAFETY: a live swap chain, serialized with the provider and resize.
        let presented = unsafe { swap_chain.GetLastPresentCount() }.ok();
        record_present(&mut state, presented, enabled);
    }
    status
}

/// The upscaler input recorded for this present (frame number and token
/// match): (generate, configure generation off).
fn matched_present(state: &mut State, frame: u64, token: Option<u64>, allow_generation: bool) -> (bool, bool) {
    match state.prepared.take() {
        Some(prepared) if prepared.matches_present(frame, token) => {
            #[cfg(feature = "diagnostics")]
            if !prepared.enabled {
                state.cadence.present_reset += 1;
            }
            (prepared.enabled && allow_generation, prepared.enabled && !allow_generation)
        }
        _unmatched => {
            #[cfg(feature = "diagnostics")]
            match _unmatched {
                Some(prepared) if prepared.frame != frame => state.cadence.present_wrong_frame += 1,
                Some(prepared) if prepared.token == token && !prepared.submitted() => {
                    state.cadence.present_before_submission += 1;
                }
                Some(_) => state.cadence.present_wrong_token += 1,
                None => state.cadence.present_no_prepare += 1,
            }
            (false, true)
        }
    }
}

fn record_present(state: &mut State, presented: Option<u32>, enabled: bool) {
    state.real += 1;
    let Some(presented) = presented else {
        state.sample = None;
        #[cfg(feature = "diagnostics")]
        {
            FRAME_RATES.store(0, Ordering::Relaxed);
            state.cadence = Cadence::default();
        }
        return;
    };
    let now = Instant::now();
    let generated = GENERATED.load(Ordering::Relaxed);
    let Some((at, real_then, presented_then, generated_then)) = state.sample else {
        state.sample = Some((now, state.real, presented, generated));
        return;
    };
    let Some(presented_delta) = presented.checked_sub(presented_then) else {
        state.sample = Some((now, state.real, presented, generated));
        return;
    };
    let elapsed = now - at;
    let seconds = elapsed.as_secs_f64();
    if seconds >= 1.0 {
        let rendered_frames = state.real - real_then;
        let presented_frames = presented_delta;
        let generated_frames = generated - generated_then;
        let sample_us = elapsed.as_micros() as u64;
        let real_fps = rendered_frames as f64 / seconds;
        let presented_fps = f64::from(presented_frames) / seconds;
        let generated_fps = generated_frames as f64 / seconds;
        #[cfg(feature = "diagnostics")]
        {
            let rendered = u64::from((real_fps as f32).to_bits());
            let presented = u64::from((presented_fps as f32).to_bits());
            let rates = if generated_fps > 0.0 {
                (rendered << 32) | presented
            } else {
                0
            };
            FRAME_RATES.store(rates, Ordering::Relaxed);
            let cadence = std::mem::take(&mut state.cadence);
            if cadence.prepare_seen != 0 || state.context.is_some() {
                tracing::info!(
                    prepare_seen = cadence.prepare_seen,
                    no_swap_chain = cadence.no_swap_chain,
                    slot_occupied = cadence.slot_occupied,
                    wrong_frame = cadence.wrong_frame,
                    first_frame_gap = ?cadence.first_frame_gap,
                    invalid_extent = cadence.invalid_extent,
                    context_unavailable = cadence.context_unavailable,
                    layout_changed = cadence.layout_changed,
                    configure_failed = cadence.configure_failed,
                    prepare_failed = cadence.prepare_failed,
                    prepared = cadence.prepared,
                    present_no_prepare = cadence.present_no_prepare,
                    present_wrong_frame = cadence.present_wrong_frame,
                    present_wrong_token = cadence.present_wrong_token,
                    present_before_submission = cadence.present_before_submission,
                    present_reset = cadence.present_reset,
                    stale = cadence.stale,
                    "frame generation: cadence"
                );
            }
        }
        tracing::info!(
            sample_us,
            rendered_frames,
            generated_frames,
            presented_frames,
            enabled,
            "frame generation: {real_fps:.1} real fps, {generated_fps:.1} generated fps, {presented_fps:.1} fps presented"
        );
        state.sample = Some((now, state.real, presented, generated));
    }
}

pub(crate) fn resize(
    frame: u64,
    width: u32,
    height: u32,
    format: i32,
    resize_buffers: impl FnOnce() -> windows::core::Result<()>,
) -> windows::core::Result<()> {
    let _retired;
    let _sdk = SDK.lock().unwrap_or_else(|e| e.into_inner());
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let drained = api().is_some_and(|api| {
        let drained = disable_and_drain(api, &state, frame);
        register_ui(api, &mut state, None);
        drained
    });
    // The proxy also waits for presents during ResizeBuffers. Even if a
    // preparatory call failed, try to restore the display and keep anything
    // the proxy might still reference alive.
    let resized = resize_buffers();
    state.prepared = None;
    for slot in &mut state.slots {
        slot.pending = None;
    }
    state.ready = None;
    state.sample = None;
    #[cfg(feature = "diagnostics")]
    {
        FRAME_RATES.store(0, Ordering::Relaxed);
        state.cadence = Cadence::default();
    }
    if drained {
        _retired = state.context.take();
    }
    state.refused_layout = None;
    if resized.is_ok() {
        state.display = [width, height];
        state.format = surface_format(format).unwrap_or(0);
    }
    resized
}

/// Release the proxy and its registered resources while the device is live.
pub fn shut_down() {
    STOPPED.store(true, Ordering::Release);
    // An earlier retirement may have left a live proxy to retry.
    if STATE.lock().unwrap_or_else(|e| e.into_inner()).swap_context == 0 {
        return;
    }
    let Some(host) = crate::host::made_host() else { return };
    tracing::info!("frame generation: waiting for submitted work before shutdown");
    crate::external::finish_submitted();
    tracing::info!("frame generation: releasing the presenter");
    host.release_framegen_presenter();
}

pub(crate) fn release_swap_chain(release_presenter: impl FnOnce()) -> bool {
    STOPPED.store(true, Ordering::Release);
    let Some(api) = api() else { return false };
    let _sdk = SDK.lock().unwrap_or_else(|e| e.into_inner());
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    retire_swap_chain(api, &mut state, release_presenter)
}

fn retire_swap_chain(api: &Api, state: &mut State, release_presenter: impl FnOnce()) -> bool {
    let mut context = state.swap_context as *mut c_void;
    if context.is_null() {
        return false;
    }
    let frame = state.last_present.unwrap_or(0);
    state.prepared = None;
    if !disable_and_drain(api, state, frame) {
        return false;
    }
    register_ui(api, state, None);
    if let Some(active) = state.context.as_mut() {
        // A recorded list can still own the provider's GPU resources even
        // after its callback was detached. Keep the presenter for a retry.
        let Some(active) = Arc::get_mut(active) else { return false };
        if !active.destroy(api) {
            return false;
        }
        state.context = None;
    }
    // SAFETY: the caller holds the presenter lock. AMD's DX12 shutdown
    // destroys its contexts before releasing the final swap-chain reference.
    let result = unsafe { (api.destroy)(&mut context, null()) };
    if result != OK {
        tracing::warn!(result, "frame generation: AMD could not destroy the swap chain");
        return false;
    }
    release_presenter();
    *state = State::default();
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct MockSwapChain {
        calls: Vec<&'static str>,
        wait_result: u32,
        destroy_result: u32,
    }

    fn mock_api() -> Api {
        unsafe extern "C" fn create(_: *mut *mut c_void, _: *mut Header, _: *const c_void) -> u32 {
            ERROR
        }
        unsafe extern "C" fn call(_: *mut *mut c_void, _: *mut Header) -> u32 {
            ERROR
        }
        unsafe extern "C" fn wait(context: *mut *mut c_void, desc: *mut Header) -> u32 {
            // SAFETY: the test supplies these live stack objects for the call.
            let (mock, desc) = unsafe { (&mut *(*context).cast::<MockSwapChain>(), &*desc) };
            if desc.kind != WAIT_FOR_PRESENTS {
                return ERROR;
            }
            mock.calls.push("drain");
            mock.wait_result
        }
        unsafe extern "C" fn destroy(context: *mut *mut c_void, _: *const c_void) -> u32 {
            // SAFETY: the test supplies a live MockSwapChain as context.
            let mock = unsafe { &mut *(*context).cast::<MockSwapChain>() };
            mock.calls.push("destroy");
            mock.destroy_result
        }
        Api { create, destroy, configure: call, query: call, dispatch: wait }
    }

    #[test]
    fn presenter_survives_a_refused_proxy_shutdown() {
        let api = mock_api();
        let mut mock = MockSwapChain { destroy_result: ERROR, ..Default::default() };
        let mock_ptr = &raw mut mock;
        let mut state = State {
            swap_context: mock_ptr as usize,
            swap_chain: 1,
            ..State::default()
        };
        let released = std::cell::Cell::new(false);
        assert!(!retire_swap_chain(&api, &mut state, || released.set(true)));
        assert!(!released.get());
        assert_eq!(state.swap_chain, 1);
        assert_eq!(mock.calls.as_slice(), ["drain", "destroy"]);

        mock.destroy_result = OK;
        assert!(retire_swap_chain(&api, &mut state, || {
            released.set(true);
            // SAFETY: the final SDK call completed before the release callback.
            let calls = unsafe { &(*mock_ptr).calls };
            assert_eq!(calls.as_slice(), ["drain", "destroy", "drain", "destroy"]);
        }));
        assert!(released.get());
        assert_eq!(state.swap_context, 0);
    }

    #[test]
    fn a_failed_drain_leaves_the_proxy_and_presenter_alive() {
        let api = mock_api();
        let mut mock = MockSwapChain { wait_result: ERROR, ..Default::default() };
        let mut state = State {
            swap_context: (&raw mut mock) as usize,
            swap_chain: 1,
            ..State::default()
        };
        assert!(!retire_swap_chain(&api, &mut state, || panic!("the SDK has live presents")));
        assert_eq!(mock.calls.as_slice(), ["drain"]);
        assert_eq!(state.swap_chain, 1);
    }

    #[test]
    fn only_a_submitted_preparation_for_this_frame_can_generate() {
        let submission = Arc::new(AtomicU8::new(RECORDED));
        let recorded = Recording(submission.clone());
        let frame = PreparedFrame { frame: 7, title_list: 1, token: Some(10), enabled: true, submission };
        assert!(!frame.matches_present(7, Some(10)));
        recorded.submitted();
        drop(recorded);
        assert!(frame.matches_present(7, Some(10)));
        assert!(!frame.matches_present(8, Some(10)));
        assert!(!frame.matches_present(7, Some(11)));
        assert!(!frame.matches_present(7, None));
    }

    fn pending(seq: u64, title_list: usize, submission: &Arc<AtomicU8>) -> Option<Pending> {
        Some(Pending {
            seq,
            title_list,
            token: None,
            submission: submission.clone(),
            depth_desc: [0; 8],
            motion_desc: [0; 8],
            max_render: [1280, 720],
            render_size: [1280, 720],
            upscale_flags: 0,
            jitter: [0.0; 2],
            motion_scale: [1.0; 2],
            frame_ms: 40.0,
            reset: false,
            camera: [0.0; 4],
        })
    }

    #[test]
    fn a_frame_recorded_before_the_previous_present_waits_for_its_own() {
        // A title held at its frame cap records frame 11's upscale (list 2)
        // before it presents frame 10 (list 1, token 10).
        let submitted = Arc::new(AtomicU8::new(SUBMITTED));
        let mut slots = vec![
            Slot { textures: None, pending: pending(1, 1, &submitted) },
            Slot { textures: None, pending: pending(2, 2, &submitted) },
        ];
        bind_slot(&mut slots, &[1], 10);
        let (taken, stale) = take_slot(&mut slots, 10);
        assert_eq!((taken.map(|(i, p)| (i, p.title_list)), stale), (Some((0, 1)), 0));
        // Frame 11 keeps its copies until its own submission and present.
        bind_slot(&mut slots, &[2], 11);
        assert_eq!(slots[1].pending.as_ref().and_then(|p| p.token), Some(11));
        let (taken, _) = take_slot(&mut slots, 11);
        assert_eq!(taken.map(|(i, _)| i), Some(1));
    }

    #[test]
    fn one_list_used_for_two_frames_binds_the_oldest_first() {
        let submitted = Arc::new(AtomicU8::new(SUBMITTED));
        let mut slots = vec![
            Slot { textures: None, pending: pending(5, 7, &submitted) },
            Slot { textures: None, pending: pending(4, 7, &submitted) },
        ];
        bind_slot(&mut slots, &[7], 20);
        assert_eq!(slots[1].pending.as_ref().and_then(|p| p.token), Some(20));
        assert_eq!(slots[0].pending.as_ref().and_then(|p| p.token), None);
    }

    #[test]
    fn a_frame_never_presented_frees_its_slot_at_a_later_present() {
        let submitted = Arc::new(AtomicU8::new(SUBMITTED));
        let mut slots = vec![
            Slot { textures: None, pending: pending(1, 1, &submitted) },
            Slot { textures: None, pending: pending(2, 2, &submitted) },
        ];
        bind_slot(&mut slots, &[1], 30);
        bind_slot(&mut slots, &[2], 31);
        let (taken, stale) = take_slot(&mut slots, 31);
        assert_eq!((taken.map(|(i, _)| i), stale), (Some(1), 1));
        assert!(slots.iter().all(|slot| slot.pending.is_none()));
    }

    #[test]
    fn a_list_reset_unsent_frees_its_slot() {
        let discarded = Arc::new(AtomicU8::new(DISCARDED));
        let mut slots = vec![Slot { textures: None, pending: pending(1, 1, &discarded) }];
        bind_slot(&mut slots, &[1], 40);
        assert_eq!(slots[0].pending.as_ref().and_then(|p| p.token), None);
        drop_discarded(&mut slots);
        assert!(slots[0].pending.is_none());
    }

    #[test]
    fn discarding_a_recording_frees_its_prepared_frame() {
        let submission = Arc::new(AtomicU8::new(RECORDED));
        drop(Recording(submission.clone()));
        assert_eq!(submission.load(Ordering::Acquire), DISCARDED);
    }

    #[test]
    fn disabled_configuration_unregisters_the_callback_and_its_context() {
        let state = State { display: [1920, 1080], ..State::default() };
        let context = std::ptr::dangling_mut::<c_void>();
        let enabled = configuration(&state, context, 7, true);
        assert!(enabled.generate_callback.is_some());
        assert_eq!(enabled.generate_context, context);
        let disabled = configuration(&state, context, 7, false);
        assert!(disabled.generate_callback.is_none());
        assert!(disabled.generate_context.is_null());
    }
}
