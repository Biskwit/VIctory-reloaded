//! The display planes after the first, drawn over it at a present as the
//! console's display composes them (`crate::queue`'s present). Grand Theft
//! Auto VI's Series S profile presents its menus and its interface in a
//! plane of their own over the game's.
//!
//! shortcut: each plane is sampled in its stored values (no colour space
//! conversion) and blended with premultiplied alpha, as the console's
//! compositor is taken to do.

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP, ID3DBlob};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::{PCSTR, s};

use crate::host::{Host, transition};

const SOURCE: &str = r#"
#define RS "RootConstants(num32BitConstants = 8, b0), DescriptorTable(SRV(t0)), StaticSampler(s0, filter = FILTER_MIN_MAG_MIP_LINEAR, addressU = TEXTURE_ADDRESS_CLAMP, addressV = TEXTURE_ADDRESS_CLAMP)"
cbuffer Place : register(b0) {
    float4 to;   // where in the window, in fractions: left, top, right, bottom
    float4 from; // what of the plane, in fractions
};
Texture2D<float4> plane : register(t0);
SamplerState linear_clamp : register(s0);
struct Corner {
    float4 at : SV_Position;
    float2 uv : TEXCOORD0;
};
[RootSignature(RS)]
Corner corners(uint i : SV_VertexID) {
    float2 k = float2(i & 1, i >> 1);
    Corner c;
    c.at = float4(lerp(to.xy, to.zw, k) * float2(2, -2) + float2(-1, 1), 0, 1);
    c.uv = lerp(from.xy, from.zw, k);
    return c;
}
[RootSignature(RS)]
float4 over(Corner c) : SV_Target {
    return plane.Sample(linear_clamp, c.uv);
}
"#;

/// A plane drawn over the first.
#[derive(Clone, Copy)]
pub(crate) struct Over<'a> {
    pub(crate) texture: &'a ID3D12Resource,
    /// The host state it is in, and stays in.
    pub(crate) state: u32,
    /// Where it goes in the window, and what of it shows, in fractions of
    /// their sizes: left, top, right, bottom.
    pub(crate) to: [f32; 4],
    pub(crate) from: [f32; 4],
}

/// Views of the planes drawn: a ring, valid long after its present ran.
const VIEWS: u32 = 64;

/// What planes are drawn with, into a window buffer of `format`.
pub(crate) struct Composer {
    format: DXGI_FORMAT,
    root: ID3D12RootSignature,
    pipeline: ID3D12PipelineState,
    views: ID3D12DescriptorHeap,
    step: u32,
    next: u32,
}

fn compile(entry: PCSTR, target: PCSTR) -> windows::core::Result<ID3DBlob> {
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    // SAFETY: the source and names live through the call; out pointers ours.
    let compiled = unsafe {
        D3DCompile(SOURCE.as_ptr().cast(), SOURCE.len(), s!("planes"), None, None, entry, target, 0, 0, &mut code, Some(&mut errors))
    };
    if let Err(e) = compiled {
        // SAFETY: the compiler's message blob.
        let text = errors.map(|b| unsafe {
            String::from_utf8_lossy(std::slice::from_raw_parts(b.GetBufferPointer().cast::<u8>(), b.GetBufferSize())).into_owned()
        });
        tracing::warn!(error = %e, ?text, "the display planes' shaders did not compile");
        return Err(e);
    }
    code.ok_or_else(windows::core::Error::empty)
}

fn bytes(blob: &ID3DBlob) -> D3D12_SHADER_BYTECODE {
    // SAFETY: the blob's own bytes, which outlive the description's use.
    unsafe {
        D3D12_SHADER_BYTECODE {
            pShaderBytecode: blob.GetBufferPointer(),
            BytecodeLength: blob.GetBufferSize(),
        }
    }
}

impl Composer {
    pub(crate) fn new(host: &Host, format: DXGI_FORMAT) -> windows::core::Result<Composer> {
        let (vs, ps) = (compile(s!("corners"), s!("vs_5_1"))?, compile(s!("over"), s!("ps_5_1"))?);
        let device = &host.device;
        let mut blend = D3D12_BLEND_DESC::default();
        blend.RenderTarget[0] = D3D12_RENDER_TARGET_BLEND_DESC {
            BlendEnable: true.into(),
            LogicOpEnable: false.into(),
            SrcBlend: D3D12_BLEND_ONE,
            DestBlend: D3D12_BLEND_INV_SRC_ALPHA,
            BlendOp: D3D12_BLEND_OP_ADD,
            SrcBlendAlpha: D3D12_BLEND_ONE,
            DestBlendAlpha: D3D12_BLEND_INV_SRC_ALPHA,
            BlendOpAlpha: D3D12_BLEND_OP_ADD,
            LogicOp: D3D12_LOGIC_OP_NOOP,
            RenderTargetWriteMask: D3D12_COLOR_WRITE_ENABLE_ALL.0 as u8,
        };
        let mut formats = [DXGI_FORMAT_UNKNOWN; 8];
        formats[0] = format;
        // SAFETY: plain object creation; the descriptions point at live
        // data and never release their pointers.
        unsafe {
            let code = bytes(&vs);
            let root: ID3D12RootSignature = device.CreateRootSignature(
                0,
                std::slice::from_raw_parts(code.pShaderBytecode.cast::<u8>(), code.BytecodeLength),
            )?;
            let pipeline = device.CreateGraphicsPipelineState(&D3D12_GRAPHICS_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                VS: code,
                PS: bytes(&ps),
                BlendState: blend,
                SampleMask: u32::MAX,
                RasterizerState: D3D12_RASTERIZER_DESC {
                    FillMode: D3D12_FILL_MODE_SOLID,
                    CullMode: D3D12_CULL_MODE_NONE,
                    DepthClipEnable: true.into(),
                    ..Default::default()
                },
                PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
                NumRenderTargets: 1,
                RTVFormats: formats,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                ..Default::default()
            })?;
            let views: ID3D12DescriptorHeap = device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                NumDescriptors: VIEWS,
                Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                NodeMask: 0,
            })?;
            let step = device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV);
            Ok(Composer { format, root, pipeline, views, step, next: 0 })
        }
    }

    /// Whether it draws into window buffers of `format`.
    pub(crate) fn draws_into(&self, format: DXGI_FORMAT) -> bool {
        self.format == format
    }

    /// Records into `list` the draws of `planes`, in order, over `back`
    /// (`width` by `height`, typed; cleared to black first with `clear`),
    /// which is in `back_state` and is left in it.
    #[allow(clippy::too_many_arguments)] // the draw's own inputs
    pub(crate) fn draw(
        &mut self,
        host: &Host,
        list: &ID3D12GraphicsCommandList,
        back: &ID3D12Resource,
        back_state: D3D12_RESOURCE_STATES,
        (width, height): (u32, u32),
        clear: bool,
        planes: &[Over],
    ) {
        const READ: D3D12_RESOURCE_STATES = D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE;
        const TARGET: D3D12_RESOURCE_STATES = D3D12_RESOURCE_STATE_RENDER_TARGET;
        // SAFETY: an open list; our objects and live resources; slots of
        // our heap.
        unsafe {
            if back_state != TARGET {
                list.ResourceBarrier(&[transition(back, back_state, TARGET)]);
            }
            let rtv = host.rtv(Some(back), None);
            if clear {
                list.ClearRenderTargetView(rtv, &[0.0, 0.0, 0.0, 1.0], None);
            }
            list.OMSetRenderTargets(1, Some(&rtv), false, None);
            list.RSSetViewports(&[D3D12_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: width as f32,
                Height: height as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            }]);
            list.RSSetScissorRects(&[RECT { left: 0, top: 0, right: width as i32, bottom: height as i32 }]);
            list.SetDescriptorHeaps(&[Some(self.views.clone())]);
            list.SetGraphicsRootSignature(&self.root);
            list.SetPipelineState(&self.pipeline);
            list.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
            for plane in planes {
                // A view in the plane's format, typed for a typeless one.
                let Some(format) = typed(plane.texture.GetDesc().Format) else {
                    continue;
                };
                let slot = self.next % VIEWS;
                self.next = self.next.wrapping_add(1);
                let mut cpu = self.views.GetCPUDescriptorHandleForHeapStart();
                cpu.ptr += (slot * self.step) as usize;
                let mut gpu = self.views.GetGPUDescriptorHandleForHeapStart();
                gpu.ptr += u64::from(slot * self.step);
                let view = D3D12_SHADER_RESOURCE_VIEW_DESC {
                    Format: format,
                    ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
                    Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                    Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                        Texture2D: D3D12_TEX2D_SRV { MipLevels: 1, ..Default::default() },
                    },
                };
                host.device.CreateShaderResourceView(plane.texture, Some(&view), cpu);
                let state = D3D12_RESOURCE_STATES(plane.state as i32);
                let moved = state.0 & READ.0 == 0;
                if moved {
                    list.ResourceBarrier(&[transition(plane.texture, state, READ)]);
                }
                let place = [plane.to, plane.from];
                list.SetGraphicsRoot32BitConstants(0, 8, place.as_ptr().cast(), 0);
                list.SetGraphicsRootDescriptorTable(1, gpu);
                list.DrawInstanced(4, 1, 0, 0);
                if moved {
                    list.ResourceBarrier(&[transition(plane.texture, READ, state)]);
                }
            }
            if back_state != TARGET {
                list.ResourceBarrier(&[transition(back, TARGET, back_state)]);
            }
        }
    }
}

/// `format` if it is typed; a typeless window format's UNORM or float
/// member; `None` for other typeless formats.
pub(crate) fn typed(format: DXGI_FORMAT) -> Option<DXGI_FORMAT> {
    use crate::resource::family;
    let f = format.0 as u32;
    if family(f) == f && family(f + 1) == f {
        crate::host::swap_format(format)
    } else {
        Some(format)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A half-transparent red plane (premultiplied) drawn over a blue
    /// window buffer gives half red, half blue.
    #[test]
    fn a_plane_is_blended_over_the_first() {
        let near = |a: u8, b: u8| a.abs_diff(b) <= 2;
        if let Some(texel) = drawn(false) {
            assert!(near(texel[0], 128) && texel[1] == 0 && near(texel[2], 128) && texel[3] == 255, "{texel:?}");
        }
    }

    /// Cleared first (the FSR 4 stand-in), the target holds the plane's
    /// colour alone: nothing of what it held stays under it.
    #[test]
    fn a_cleared_target_holds_the_plane_alone() {
        let near = |a: u8, b: u8| a.abs_diff(b) <= 2;
        if let Some(texel) = drawn(true) {
            assert!(near(texel[0], 128) && texel[1] == 0 && texel[2] == 0 && texel[3] == 255, "{texel:?}");
        }
    }

    /// The first texel of a blue 4x4 target with a half-transparent red
    /// plane drawn over it (cleared to black first with `clear`); `None`
    /// without a host GPU.
    fn drawn(clear: bool) -> Option<[u8; 4]> {
        let host = crate::host::host()?;
        let target = |state| {
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                Width: 4,
                Height: 4,
                DepthOrArraySize: 1,
                MipLevels: 1,
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
                ..Default::default()
            };
            let heap = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() };
            let mut made: Option<ID3D12Resource> = None;
            // SAFETY: plain creation of a texture.
            unsafe { host.device.CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, state, None, &mut made) }.unwrap();
            made.unwrap()
        };
        let (back, plane) = (target(D3D12_RESOURCE_STATE_RENDER_TARGET), target(D3D12_RESOURCE_STATE_RENDER_TARGET));
        let heap = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_READBACK, ..Default::default() };
        let rows = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: 1024,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut readback: Option<ID3D12Resource> = None;
        // SAFETY: plain creation of a readback buffer.
        unsafe {
            host.device
                .CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &rows, D3D12_RESOURCE_STATE_COPY_DEST, None, &mut readback)
                .unwrap();
        }
        let readback = readback.unwrap();
        let mut composer = Composer::new(host, DXGI_FORMAT_R8G8B8A8_UNORM).unwrap();
        let list = host.new_list().unwrap();
        let l = &list.list;
        let to = D3D12_TEXTURE_COPY_LOCATION {
            pResource: std::mem::ManuallyDrop::new(Some(readback.clone())),
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                    Offset: 0,
                    Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                        Width: 4,
                        Height: 4,
                        Depth: 1,
                        RowPitch: 256,
                    },
                },
            },
        };
        let from = D3D12_TEXTURE_COPY_LOCATION {
            pResource: std::mem::ManuallyDrop::new(Some(back.clone())),
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
        };
        // SAFETY: an open list; live resources and fresh views of them.
        unsafe {
            l.ClearRenderTargetView(host.rtv(Some(&back), None), &[0.0, 0.0, 1.0, 1.0], None);
            l.ClearRenderTargetView(host.rtv(Some(&plane), None), &[0.5, 0.0, 0.0, 0.5], None);
            l.ResourceBarrier(&[transition(&back, D3D12_RESOURCE_STATE_RENDER_TARGET, D3D12_RESOURCE_STATE_COPY_DEST)]);
            let over = Over {
                texture: &plane,
                state: D3D12_RESOURCE_STATE_RENDER_TARGET.0 as u32,
                to: [0.0, 0.0, 1.0, 1.0],
                from: [0.0, 0.0, 1.0, 1.0],
            };
            composer.draw(host, l, &back, D3D12_RESOURCE_STATE_COPY_DEST, (4, 4), clear, &[over]);
            l.ResourceBarrier(&[transition(&back, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
            l.CopyTextureRegion(&to, 0, 0, 0, &from, None);
        }
        list.close();
        host.execute(&[&list]);
        let mut mapped = std::ptr::null_mut();
        // SAFETY: the copy has run (`execute` waits); 1024 bytes are mapped.
        let texel = unsafe {
            readback.Map(0, None, Some(&mut mapped)).unwrap();
            mapped.cast::<[u8; 4]>().read()
        };
        // SAFETY: mapped above.
        unsafe { readback.Unmap(0, None) };
        drop(std::mem::ManuallyDrop::into_inner(to.pResource));
        drop(std::mem::ManuallyDrop::into_inner(from.pResource));
        Some(texel)
    }
}
