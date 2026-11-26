//! Diagnostics: what the control file (`crate::control`) adds at a present:
//! a shot of the window, and a view (`view <n>`): one texture of the frame
//! in place of the frame, its values brought to what a screen shows as
//! target dumps are (`crate::capture`), from the range of the channels
//! shown, which a compute pass over the texture finds first. The range found
//! is read back and named under the overlay's table at the next present.
//!
//! The view's objects are made at the first view. A view makes its present
//! wait for the card (its views and range buffer are used again at the next
//! one).

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::s;

use super::{Constants, DRAW, Overlay, Pipelines, bytecode, bytes, compile, target};
use crate::control::{AtPresent, Channel, Viewed};
use crate::host::{Host, HostList};

/// The view's root parameters after the constants: the range, the table of
/// the viewed texture.
const RANGE: u32 = 1;
const TEXTURE: u32 = 2;

/// How a view shows values (`SHOW_*` in overlay.hlsl); 0: as they are.
const SHOW_RANGE: u32 = 1;
const SHOW_HIGHEST: u32 = 2;
const SHOW_CURVE: u32 = 4;

/// Texels a side the range pass reads at most: a step between them past that.
const RANGE_GRID: u32 = 2048;

/// How a view reads a texture of host format `format` and shows it: the
/// view's format, whether it holds integers, the channel of a format of one
/// channel (red, or alpha for A8), and how values are shown (`SHOW_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Reading {
    format: u32,
    integers: bool,
    single: Option<u32>,
    show: u32,
}

/// [`Reading`] for host format `format`, as dumps show it
/// (`crate::capture`): 8-bit and compressed colour as it is; other colour
/// scaled by its highest value, floats of light through a cube root; a
/// single channel and depth from their lowest to their highest. Typeless
/// formats are read as their plain kind (depth as its depth plane). `None`
/// for formats not shown (signed integers, typed depth, video).
fn reading(format: u32) -> Option<Reading> {
    const LIGHT: u32 = SHOW_HIGHEST | SHOW_CURVE;
    let colour = |format, show| Reading {
        format,
        integers: false,
        single: None,
        show,
    };
    let integers = |format| Reading {
        format,
        integers: true,
        single: None,
        show: SHOW_HIGHEST,
    };
    let single = |format, integers| Reading {
        format,
        integers,
        single: Some(0),
        show: SHOW_RANGE,
    };
    Some(match format {
        1 | 2 => colour(2, LIGHT),
        3 => integers(3),
        5 | 6 => colour(6, LIGHT),
        7 => integers(7),
        9 | 10 => colour(10, LIGHT),
        11 | 13 => colour(format, SHOW_HIGHEST),
        12 => integers(12),
        15 | 16 => colour(16, SHOW_HIGHEST),
        17 => integers(17),
        // Depth planes: R32G8X24, R24G8, R16 typeless (and their views).
        19 | 21 => single(21, false),
        23 | 24 | 89 => colour(if format == 23 { 24 } else { format }, SHOW_HIGHEST),
        25 => integers(25),
        26 | 67 => colour(format, LIGHT),
        27 | 28 => colour(28, 0),
        29 | 31 => colour(format, 0),
        30 => integers(30),
        33 | 34 => colour(34, SHOW_HIGHEST),
        35 | 37 => colour(format, SHOW_HIGHEST),
        36 => integers(36),
        39 | 41 => single(41, false),
        42 => single(42, true),
        44 | 46 => single(46, false),
        48 | 49 => colour(49, 0),
        50 => integers(50),
        51 => colour(51, 0),
        53 | 56 => single(56, false),
        54 | 58 => single(format, false),
        57 => single(57, true),
        60 | 61 => single(61, false),
        62 => single(62, true),
        63 => single(63, false),
        65 => Reading {
            single: Some(3),
            ..single(65, false)
        },
        // Block-compressed: typeless as UNORM; BC4 has one channel; BC6H
        // holds light.
        70..=78 | 82..=84 | 97..=99 => colour(
            if matches!(format, 70 | 73 | 76 | 82 | 97) {
                format + 1
            } else {
                format
            },
            0,
        ),
        79..=81 => Reading {
            single: Some(0),
            ..colour(if format == 79 { 80 } else { format }, 0)
        },
        94..=96 => colour(if format == 94 { 95 } else { format }, LIGHT),
        85 | 86 | 87 | 88 | 91 | 93 | 115 => colour(format, 0),
        90 | 92 => colour(format - 3, 0),
        _ => return None,
    })
}

/// The channel shown (0-3; 4: red, green and blue) and how, for `channel`
/// asked of a texture read as `r`: one channel asked for is shown as grey,
/// from its lowest to its highest unless the format is shown as it is.
fn shown(r: Reading, channel: Channel) -> (u32, u32) {
    let one = match channel {
        Channel::R => 0,
        Channel::G => 1,
        Channel::B => 2,
        Channel::A => 3,
        Channel::Rgb => return (r.single.unwrap_or(4), r.show),
    };
    let show = if r.show == 0 {
        0
    } else {
        SHOW_RANGE | r.show & SHOW_CURVE
    };
    (one, show)
}

/// A float from the range pass's key (`unkey` in overlay.hlsl).
fn unkey(k: u32) -> f32 {
    f32::from_bits(if k & 0x8000_0000 != 0 {
        k & 0x7fff_ffff
    } else {
        !k
    })
}

/// What a present still has to do once its list has run ([`finish`]).
pub(crate) struct After {
    shot: Option<(String, ID3D12Resource, D3D12_PLACED_SUBRESOURCE_FOOTPRINT)>,
    range: bool,
}

/// The objects a view draws with.
pub(crate) struct View {
    pipelines: Pipelines,
    reset: ID3D12PipelineState,
    reduce: ID3D12PipelineState,
    /// The range pass's lowest and highest key, where it is read back to,
    /// and what that held after the last view.
    range: ID3D12Resource,
    range_back: ID3D12Resource,
    last_range: Option<(String, f32, f32)>,
    /// The label of the view whose range is being read back.
    ranging: String,
    /// The viewed texture's two views (floats, integers).
    views: ID3D12DescriptorHeap,
    views_cpu: usize,
    views_gpu: u64,
    view_step: u32,
}

impl View {
    fn new(host: &Host) -> Option<View> {
        match Self::make(host) {
            Ok(made) => made,
            Err(e) => {
                tracing::warn!(error = %e, "no view");
                None
            }
        }
    }

    fn make(host: &Host) -> Result<Option<View>, String> {
        let device = &host.device;
        let (Some(range), Some(range_back)) = (host.uav_buffer(8), readback(host, 8)) else {
            return Ok(None);
        };
        let pipelines = Pipelines::new(host, [s!("view_corners"), s!("view")], false)?;
        let [reset, reduce] = [
            compile(s!("reset"), s!("cs_5_1"))?,
            compile(s!("reduce"), s!("cs_5_1"))?,
        ];
        // SAFETY: the descriptions point at live data and never release
        // their pointers.
        unsafe {
            let compute = |code: &[u8]| {
                device.CreateComputePipelineState::<ID3D12PipelineState>(
                    &D3D12_COMPUTE_PIPELINE_STATE_DESC {
                        pRootSignature: std::mem::transmute_copy(&pipelines.root),
                        CS: bytecode(code),
                        ..Default::default()
                    },
                )
            };
            let (reset, reduce) = (compute(bytes(&reset)), compute(bytes(&reduce)));
            let (reset, reduce) = (
                reset.map_err(|e| e.to_string())?,
                reduce.map_err(|e| e.to_string())?,
            );
            let kind = D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV;
            let views: ID3D12DescriptorHeap = device
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: kind,
                    NumDescriptors: 2,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                    NodeMask: 0,
                })
                .map_err(|e| e.to_string())?;
            let view = View {
                pipelines,
                reset,
                reduce,
                range,
                range_back,
                last_range: None,
                ranging: String::new(),
                views_cpu: views.GetCPUDescriptorHandleForHeapStart().ptr,
                views_gpu: views.GetGPUDescriptorHandleForHeapStart().ptr,
                view_step: device.GetDescriptorHandleIncrementSize(kind),
                views,
            };
            tracing::info!("view made");
            Ok(Some(view))
        }
    }

    /// Writes the table's two views: of `texture` read as `r` (in the slot
    /// of its kind) and a null view.
    fn texture_views(&self, host: &Host, texture: &ID3D12Resource, r: Reading) {
        let desc = |format: DXGI_FORMAT| D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: format,
            ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2DARRAY,
            Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture2DArray: D3D12_TEX2D_ARRAY_SRV {
                    MostDetailedMip: 0,
                    MipLevels: 1,
                    FirstArraySlice: 0,
                    ArraySize: 1,
                    ..Default::default()
                },
            },
        };
        let slot = |i: u32| D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: self.views_cpu + (i * self.view_step) as usize,
        };
        let nulls = [DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_FORMAT_R32G32B32A32_UINT];
        for (i, null) in nulls.into_iter().enumerate() {
            // SAFETY: a slot of our heap; a live texture, or a null view.
            unsafe {
                if usize::from(r.integers) == i {
                    host.device.CreateShaderResourceView(
                        texture,
                        Some(&desc(DXGI_FORMAT(r.format as i32))),
                        slot(i as u32),
                    );
                } else {
                    host.device.CreateShaderResourceView(
                        None::<&ID3D12Resource>,
                        Some(&desc(null)),
                        slot(i as u32),
                    );
                }
            }
        }
    }

    /// Binds the root signature and its arguments for graphics (or compute).
    fn bind(&self, list: &ID3D12GraphicsCommandList, constants: &Constants, graphics: bool) {
        let words = (size_of::<Constants>() / 4) as u32;
        let table = D3D12_GPU_DESCRIPTOR_HANDLE {
            ptr: self.views_gpu,
        };
        let root = &self.pipelines.root;
        // SAFETY: an open list; our root signature, heap and buffers, which
        // live as long as the view.
        unsafe {
            list.SetDescriptorHeaps(&[Some(self.views.clone())]);
            let constants = std::ptr::from_ref(constants).cast();
            if graphics {
                list.SetGraphicsRootSignature(root);
                list.SetGraphicsRoot32BitConstants(DRAW, words, constants, 0);
                list.SetGraphicsRootUnorderedAccessView(RANGE, self.range.GetGPUVirtualAddress());
                list.SetGraphicsRootDescriptorTable(TEXTURE, table);
            } else {
                list.SetComputeRootSignature(root);
                list.SetComputeRoot32BitConstants(DRAW, words, constants, 0);
                list.SetComputeRootUnorderedAccessView(RANGE, self.range.GetGPUVirtualAddress());
                list.SetComputeRootDescriptorTable(TEXTURE, table);
            }
        }
    }

    /// Records the view of `viewed` over the whole of the window buffer
    /// (`rtv`, `size`, `format`; in the render target state): the range
    /// pass, the texture scaled to fit, the range's read back.
    fn view(
        &mut self,
        host: &Host,
        list: &ID3D12GraphicsCommandList,
        rtv: D3D12_CPU_DESCRIPTOR_HANDLE,
        format: DXGI_FORMAT,
        size: [u32; 2],
        viewed: &Viewed,
    ) -> Result<(), String> {
        use std::sync::atomic::Ordering::Relaxed;
        let texture = &viewed.copy.texture;
        // SAFETY: a live resource.
        let d = unsafe { texture.GetDesc() };
        if d.Dimension != D3D12_RESOURCE_DIMENSION_TEXTURE2D || d.SampleDesc.Count > 1 {
            return Err("not shown: not a 2D texture of one sample".to_owned());
        }
        if d.Flags.0 & D3D12_RESOURCE_FLAG_DENY_SHADER_RESOURCE.0 != 0 {
            return Err("not shown: shaders may not read it".to_owned());
        }
        let r = reading(d.Format.0 as u32)
            .ok_or_else(|| format!("not shown: format {}", d.Format.0))?;
        let view = self.pipelines.get(host, format).ok_or("no pipeline")?;
        let (channel, show) = shown(r, viewed.channel);
        let (width, height) = (d.Width as u32, d.Height);
        let step = width.max(height).div_ceil(RANGE_GRID).max(1);
        // The texture scaled to fit the window, in its middle.
        let scale = (size[0] as f32 / width as f32).min(size[1] as f32 / height as f32);
        let (w, h) = (width as f32 * scale, height as f32 * scale);
        let (left, top) = ((size[0] as f32 - w) / 2.0, (size[1] as f32 - h) / 2.0);
        let mut constants = Constants {
            rect: [left, top, left + w, top + h],
            target: [size[0] as f32, size[1] as f32],
            head: [channel, show, 0, 0],
            ..Default::default()
        };
        constants.words[..4].copy_from_slice(&[width, height, u32::from(r.integers), step]);
        self.texture_views(host, texture, r);
        self.ranging.clone_from(&viewed.label);
        let state = D3D12_RESOURCE_STATES(crate::host::legal_state(
            texture,
            viewed.copy.state.load(Relaxed),
        ) as i32);
        let read = D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE
            | D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE;
        let uav = D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                    // SAFETY: a borrowed pointer the barrier never releases.
                    pResource: unsafe { std::mem::transmute_copy(&self.range) },
                }),
            },
        };
        let common = D3D12_RESOURCE_STATE_COMMON;
        // SAFETY: an open list; live resources; every state as recorded.
        unsafe {
            let mut barriers = vec![crate::host::transition(
                &self.range,
                common,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            )];
            if state != read {
                barriers.push(crate::host::transition(texture, state, read));
            }
            list.ResourceBarrier(&barriers);
            self.bind(list, &constants, false);
            list.SetPipelineState(&self.reset);
            list.Dispatch(1, 1, 1);
            list.ResourceBarrier(std::slice::from_ref(&uav));
            list.SetPipelineState(&self.reduce);
            list.Dispatch(
                width.div_ceil(step).div_ceil(8),
                height.div_ceil(step).div_ceil(8),
                1,
            );
            list.ResourceBarrier(std::slice::from_ref(&uav));
            self.bind(list, &constants, true);
            target(list, rtv, size);
            list.ClearRenderTargetView(rtv, &[0.04, 0.04, 0.06, 1.0], None);
            list.SetPipelineState(&view);
            list.DrawInstanced(4, 1, 0, 0);
            let mut barriers = vec![crate::host::transition(
                &self.range,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            )];
            if state != read {
                barriers.push(crate::host::transition(texture, read, state));
            }
            list.ResourceBarrier(&barriers);
            list.CopyBufferRegion(&self.range_back, 0, &self.range, 0, 8);
            list.ResourceBarrier(&[crate::host::transition(
                &self.range,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
                common,
            )]);
        }
        Ok(())
    }

    /// After a view's list ran: the range its pass found.
    fn read_range(&mut self) {
        let mut data = std::ptr::null_mut();
        // SAFETY: the copy finished (the list was waited for); the mapping
        // holds the two keys.
        unsafe {
            if self.range_back.Map(0, None, Some(&mut data)).is_err() {
                return;
            }
            let [low, high] = data.cast::<[u32; 2]>().read();
            self.range_back.Unmap(0, None);
            self.last_range =
                (low <= high).then(|| (self.ranging.clone(), unkey(low), unkey(high)));
        }
    }
}

/// A buffer of `bytes` the CPU reads back, in the copy destination state.
fn readback(host: &Host, bytes: u64) -> Option<ID3D12Resource> {
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_READBACK,
        ..Default::default()
    };
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: bytes,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        ..Default::default()
    };
    let mut buffer: Option<ID3D12Resource> = None;
    // SAFETY: plain creation; the out pointer is ours.
    unsafe {
        host.device
            .CreateCommittedResource(
                &heap,
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATE_COPY_DEST,
                None,
                &mut buffer,
            )
            .ok()?;
    }
    buffer
}

/// At a present, as `at` asks: records into `list`, after the frame's copy
/// into the window buffer `back` (in the copy destination state, where it
/// is left), the view in place of the frame, the shot's read back (of what
/// the window shows without the overlay), and the overlay (`line`'s table
/// if it is on, the view's line under it). `made` holds the overlay's
/// objects. What is left to do after the list ran ([`finish`]), if
/// anything.
pub(crate) fn record(
    made: &mut Option<Option<Overlay>>,
    host: &Host,
    list: &HostList,
    back: &ID3D12Resource,
    line: &super::FrameLine,
    at: AtPresent,
) -> Option<After> {
    // Not the title's: the frame recorder leaves it out.
    let _ours = crate::replay::record::unrecorded();
    let mut overlay = made.get_or_insert_with(|| Overlay::new(host)).as_mut();
    let list = &list.list;
    // SAFETY: a live resource.
    let d = unsafe { back.GetDesc() };
    let (format, size) = (d.Format, [d.Width as u32, d.Height]);
    let mut state = D3D12_RESOURCE_STATE_COPY_DEST;
    let mut go = |to: D3D12_RESOURCE_STATES| {
        if state != to {
            // SAFETY: an open list and the window's live buffer.
            unsafe { list.ResourceBarrier(&[crate::host::transition(back, state, to)]) };
            state = to;
        }
    };
    let mut after = After {
        shot: None,
        range: false,
    };
    // The view's line: what it shows and the range of values it found at
    // the last present that showed the same, or why it shows nothing.
    let mut note = None;
    match (&at.view, overlay.as_deref_mut()) {
        (Some(Ok(viewed)), Some(o)) => {
            go(D3D12_RESOURCE_STATE_RENDER_TARGET);
            let view = o.view.get_or_insert_with(|| View::new(host)).as_mut();
            // A view of the window buffer from the host's ring, made where
            // it is recorded (the ring comes round).
            let shown = view.ok_or_else(|| "no view".to_owned()).and_then(|v| {
                v.view(host, list, host.rtv(Some(back), None), format, size, viewed)?;
                Ok(v)
            });
            note = Some(match shown {
                Ok(v) => {
                    after.range = true;
                    match v
                        .last_range
                        .as_ref()
                        .filter(|(label, _, _)| *label == viewed.label)
                    {
                        Some((_, low, high)) => {
                            format!("{}: values {low:.4e} to {high:.4e}", viewed.label)
                        }
                        None => viewed.label.clone(),
                    }
                }
                Err(why) => format!("{}: {why}", viewed.label),
            });
        }
        (Some(Err(why)), _) => note = Some(why.clone()),
        _ => {}
    }
    if let Some(name) = &at.shot {
        go(D3D12_RESOURCE_STATE_COPY_SOURCE);
        after.shot = crate::capture::read_back(&host.device, list, back)
            .map(|(buffer, footprint)| (name.clone(), buffer, footprint));
    }
    if let Some(o) = overlay
        && (at.overlay || note.is_some())
    {
        go(D3D12_RESOURCE_STATE_RENDER_TARGET);
        o.panel(
            host,
            list,
            back,
            at.overlay.then_some(line),
            note.as_deref(),
        );
    }
    go(D3D12_RESOURCE_STATE_COPY_DEST);
    (after.shot.is_some() || after.range).then_some(after)
}

/// After the list [`record`] recorded into ran: saves the shot, reads the
/// view's range back.
pub(crate) fn finish(made: &mut Option<Option<Overlay>>, after: After) {
    if let Some((name, buffer, footprint)) = &after.shot {
        crate::capture::save_shot(name, buffer, footprint);
    }
    if after.range
        && let Some(Some(o)) = made
        && let Some(Some(v)) = &mut o.view
    {
        v.read_range();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Formats are read as dumps show them: depth by its plane, one channel
    /// as grey from its range, light through the curve, 8-bit as it is.
    #[test]
    fn views_read_formats_as_dumps_show_them() {
        let r = |f| reading(f).unwrap();
        assert_eq!(
            r(44),
            Reading {
                format: 46,
                integers: false,
                single: Some(0),
                show: SHOW_RANGE
            }
        );
        assert_eq!(r(39).format, 41);
        assert_eq!(r(10).show, SHOW_HIGHEST | SHOW_CURVE);
        assert_eq!(
            r(27),
            Reading {
                format: 28,
                integers: false,
                single: None,
                show: 0
            }
        );
        assert!(r(42).integers);
        assert_eq!(r(65).single, Some(3));
        assert_eq!(reading(40), None, "typed depth has no shader view");
        assert_eq!(shown(r(10), Channel::Rgb), (4, SHOW_HIGHEST | SHOW_CURVE));
        assert_eq!(shown(r(10), Channel::A), (3, SHOW_RANGE | SHOW_CURVE));
        assert_eq!(shown(r(44), Channel::Rgb), (0, SHOW_RANGE));
        assert_eq!(shown(r(28), Channel::G), (1, 0));
    }

    /// The range pass's keys order as the floats do.
    #[test]
    fn range_keys_order_as_floats() {
        let key = |f: f32| {
            let u = f.to_bits();
            if u & 0x8000_0000 != 0 {
                !u
            } else {
                u | 0x8000_0000
            }
        };
        let values = [-1e30f32, -2.5, -0.0, 0.0, 1e-20, 0.5, 3.0, 1e30];
        for pair in values.windows(2) {
            assert!(key(pair[0]) <= key(pair[1]), "{pair:?}");
        }
        for v in values {
            assert_eq!(unkey(key(v)).to_bits(), v.to_bits());
        }
    }
}
