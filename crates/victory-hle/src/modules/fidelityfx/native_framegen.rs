//! Observe the title's FidelityFX upscaler without replacing its provider.

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::sync::{Arc, Mutex, OnceLock};

use victory_gpu::{external, object::Object};

use super::{
    CREATE_UPSCALE, Call, Create, CreateUpscale, DISPATCH_UPSCALE, Destroy, DispatchUpscale,
    Header, NO_PROVIDER, OK, dispatch_output, minimum_extent, record_framegen,
};

struct Api {
    create: Create,
    destroy: Destroy,
    dispatch: Call,
}

struct Context {
    flags: u32,
    max_render: [u32; 2],
    max_output: [u32; 2],
}

static API: OnceLock<Api> = OnceLock::new();
static CONTEXTS: Mutex<BTreeMap<usize, Arc<Context>>> = Mutex::new(BTreeMap::new());

/// Install the observer before binding and executing the main image.
///
/// # Safety
/// The addresses must be the three FidelityFX exports, mapped for the whole run.
pub unsafe fn install(create: usize, destroy: usize, dispatch: usize) -> Result<(), &'static str> {
    if create == 0 || destroy == 0 || dispatch == 0 {
        return Err("native FidelityFX entry point is missing");
    }
    // SAFETY: the caller supplies the mapped FidelityFX exports with these signatures.
    let api = unsafe {
        Api {
            create: std::mem::transmute::<usize, Create>(create),
            destroy: std::mem::transmute::<usize, Destroy>(destroy),
            dispatch: std::mem::transmute::<usize, Call>(dispatch),
        }
    };
    API.set(api)
        .map_err(|_| "native FidelityFX observer already installed")
}

pub fn replacement(name: &str) -> Option<usize> {
    API.get()?;
    match name {
        "ffxCreateContext" => Some(create_context as Create as *const () as usize),
        "ffxDestroyContext" => Some(destroy_context as Destroy as *const () as usize),
        "ffxDispatch" => Some(dispatch as Call as *const () as usize),
        _ => None,
    }
}

unsafe extern "C" fn create_context(
    context: *mut *mut c_void,
    desc: *mut Header,
    allocation: *const c_void,
) -> u32 {
    let Some(api) = API.get() else {
        return NO_PROVIDER;
    };
    // SAFETY: FidelityFX's caller supplies a descriptor of the stated type.
    let upscale = unsafe {
        desc.as_ref()
            .filter(|desc| desc.kind == CREATE_UPSCALE)
            .map(|_| {
                let desc = &*desc.cast::<CreateUpscale>();
                Context {
                    flags: desc.flags,
                    max_render: desc.max_render,
                    max_output: desc.max_output,
                }
            })
    };
    // SAFETY: pass through the title's arguments to their original destination.
    let result = unsafe { (api.create)(context, desc, allocation) };
    if result == OK
        && let Some(upscale) = upscale
        // SAFETY: on success the native loader populated the supplied context slot.
        && let Some(&handle) = unsafe { context.as_ref() }
        && !handle.is_null()
    {
        CONTEXTS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(handle as usize, Arc::new(upscale));
    }
    result
}

fn lookup(handle: usize) -> Option<Arc<Context>> {
    CONTEXTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&handle)
        .cloned()
}

unsafe extern "C" fn destroy_context(context: *mut *mut c_void, allocation: *const c_void) -> u32 {
    let Some(api) = API.get() else {
        return NO_PROVIDER;
    };
    // SAFETY: the native caller supplies a valid input context slot or null.
    let handle = unsafe { context.as_ref() }.map_or(0, |handle| *handle as usize);
    let tracked = lookup(handle);
    // SAFETY: the native loader owns the context and receives the original arguments.
    let result = unsafe { (api.destroy)(context, allocation) };
    if result == OK
        && let Some(tracked) = tracked
    {
        let mut contexts = CONTEXTS.lock().unwrap_or_else(|e| e.into_inner());
        if contexts
            .get(&handle)
            .is_some_and(|ctx| Arc::ptr_eq(ctx, &tracked))
        {
            contexts.remove(&handle);
        }
    }
    result
}

unsafe extern "C" fn dispatch(context: *mut *mut c_void, desc: *mut Header) -> u32 {
    let Some(api) = API.get() else {
        return NO_PROVIDER;
    };
    // SAFETY: FidelityFX's caller supplies a descriptor of the stated type.
    let upscale = unsafe {
        desc.as_ref()
            .filter(|desc| desc.kind == DISPATCH_UPSCALE)
            .map(|_| desc.cast::<DispatchUpscale>().read())
    };
    // SAFETY: the native caller supplies a valid input context slot or null.
    let tracked = upscale
        .as_ref()
        .and_then(|_| lookup(unsafe { context.as_ref() }.map_or(0, |handle| *handle as usize)));
    // SAFETY: the native loader owns the context and receives the original arguments.
    let result = unsafe { (api.dispatch)(context, desc) };
    if result == OK
        && let (Some(ctx), Some(upscale)) = (tracked, upscale)
    {
        let prepared = external::record(upscale.list.cast::<Object>().cast_const(), |list| {
            prepare(list, &ctx, &upscale)
        });
        if prepared.flatten().is_none() {
            once!(
                warn,
                "frame generation: native upscaler inputs are not available on the host list"
            );
        }
    }
    result
}

fn prepare(list: &external::Recording<'_>, ctx: &Context, upscale: &DispatchUpscale) -> Option<()> {
    let render = upscale.render_size;
    let output = dispatch_output(render, upscale.upscale_size, ctx.max_render, ctx.max_output)?;
    let depth = upscale.resources[1].resource.cast::<Object>().cast_const();
    let motion = upscale.resources[2].resource.cast::<Object>().cast_const();
    let (depth_id, depth_state) = list.existing_texture_binding(depth)?;
    let (motion_id, motion_state) = list.existing_texture_binding(motion)?;
    let (depth_state, motion_state) = (depth_state?, motion_state?);
    if depth_id == motion_id {
        return None;
    }
    const NON_PIXEL_SHADER_RESOURCE: u32 = 0x40;
    const FFX_COMPUTE_READ: u32 = 0x4;
    let depth_read = list.texture(depth, NON_PIXEL_SHADER_RESOURCE);
    let motion_read = list.texture(motion, NON_PIXEL_SHADER_RESOURCE);
    let prepared = (|| {
        let (depth, NON_PIXEL_SHADER_RESOURCE) = depth_read? else {
            return None;
        };
        let (motion, NON_PIXEL_SHADER_RESOURCE) = motion_read? else {
            return None;
        };
        for (index, texture) in [(1, &depth), (2, &motion)] {
            let [width, height] = minimum_extent(index, ctx.flags, render, output);
            // SAFETY: a live host resource held by the command list.
            let desc = unsafe { texture.GetDesc() };
            if desc.Width < u64::from(width) || desc.Height < height {
                return None;
            }
        }
        once!(
            info,
            ?render,
            ?output,
            "frame generation: captured native FidelityFX depth and motion"
        );
        record_framegen(
            list,
            upscale,
            ctx.max_render,
            ctx.flags,
            [&depth, &motion],
            [FFX_COMPUTE_READ; 2],
        );
        Some(())
    })();
    let _ = list.texture(motion, motion_state);
    let _ = list.texture(depth, depth_state);
    prepared
}
