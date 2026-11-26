// What the host draws over the presented frame (`crate::overlay`): a panel
// of filled rectangles and text from a font's atlas, and in diagnostics
// builds the view of one texture of the frame in place of the frame.
// Compiled entry point by entry point with D3DCompile (shader model 5.1)
// when the overlay first draws, as `crate::compose`'s shaders are; nothing
// compiled is kept in the repository.
//
// The panel's root signature: the draw's 44 constants and the font's atlas
// as a root view. The view's: the same constants, the viewed texture's range of
// values as a root unordered view, and a table of the viewed texture read as
// floats and as integers (whichever does not fit its format is a null view).
#define PANEL_RS "RootConstants(num32BitConstants = 44, b0), SRV(t0)"
#define VIEW_RS "RootConstants(num32BitConstants = 44, b0), UAV(u0), " \
    "DescriptorTable(SRV(t1, numDescriptors = 2, flags = DESCRIPTORS_VOLATILE))"

cbuffer Draw : register(b0) {
    // The rectangle drawn, in the target's pixels: left, top, right, bottom.
    float4 rect;
    // The target's size in pixels.
    float2 target;
    // Panel: the colour of the text or of a filled rectangle (8 bits a
    // channel, red lowest, alpha highest), and the cell of the text's face
    // in pixels: width in bits 0-7, height in bits 8-15 (0: filled).
    uint2 style;
    // Panel: the text's characters, and the byte of the atlas where its
    // face starts. View: the channel shown (0-3; 4: red, green and blue),
    // and how (SHOW_*).
    uint4 head;
    // Panel: the characters, four a word, the first in the low byte. View:
    // words[0] = the texture's width and height, 1 if it holds integers,
    // and the step between the texels the range pass reads.
    uint4 words[8];
};

// The font's atlas: faces of characters 32 to 126, a byte of coverage a
// pixel, glyph after glyph, each row after row (`crate::overlay::atlas`).
ByteAddressBuffer font : register(t0);
// The lowest and highest value of the viewed channels, as ordered keys.
RWByteAddressBuffer range : register(u0);
Texture2DArray<float4> floats : register(t1);
Texture2DArray<uint4> integers : register(t2);

// A view's values are shifted and scaled from the range's lowest and
// highest to 0 and 1, or scaled by the highest alone, then maybe go
// through a cube root (light, whose bright few would leave the rest black).
static const uint SHOW_RANGE = 1;
static const uint SHOW_HIGHEST = 2;
static const uint SHOW_CURVE = 4;

float4 place(uint i) {
    float2 at = lerp(rect.xy, rect.zw, float2(i & 1, i >> 1));
    return float4(at / target * float2(2, -2) + float2(-1, 1), 0, 1);
}

[RootSignature(PANEL_RS)]
float4 corners(uint i : SV_VertexID) : SV_Position {
    return place(i);
}

float4 colour(uint c) {
    return float4(c & 0xff, (c >> 8) & 0xff, (c >> 16) & 0xff, c >> 24) / 255.0;
}

// A filled rectangle, or a line of text whose rectangle starts at its first
// cell's top left, a pixel of the atlas a pixel; the ink is blended by the
// glyph's coverage, and what it does not cover is left as it is.
[RootSignature(PANEL_RS)]
float4 panel(float4 p : SV_Position) : SV_Target {
    uint2 cell = uint2(style.y & 0xff, (style.y >> 8) & 0xff);
    float4 ink = colour(style.x);
    if (cell.x == 0) {
        return ink;
    }
    uint2 at = uint2(p.xy - rect.xy);
    uint column = at.x / cell.x;
    if (column >= head.x || at.y >= cell.y) {
        discard;
    }
    // A discarded lane runs on (the helper lanes of a quad at the text's
    // edge, outside its rectangle): what it reads stays in the face. The
    // font is a root view, read without bounds: past a text of an odd
    // width, the character after it (0) put the big face's reads before the
    // atlas, and the card hung (run rb-ovl1).
    column = min(column, max(head.x, 1) - 1);
    at.y = min(at.y, cell.y - 1);
    uint c = clamp((words[column / 16][(column / 4) % 4] >> ((column % 4) * 8)) & 0xff, 32, 126);
    uint byte = head.y + ((c - 32) * cell.y + at.y) * cell.x + at.x % cell.x;
    uint covered = (font.Load(byte & ~3u) >> ((byte & 3) * 8)) & 0xff;
    if (covered == 0) {
        discard;
    }
    return float4(ink.rgb, ink.a * covered / 255.0);
}

[RootSignature(VIEW_RS)]
float4 view_corners(uint i : SV_VertexID) : SV_Position {
    return place(i);
}

float4 fetch(int2 at) {
    if (words[0].z != 0) {
        return float4(integers.Load(int4(at, 0, 0)));
    }
    return floats.Load(int4(at, 0, 0));
}

// Floats as keys that order as the floats do, for atomic minimum and maximum.
uint key(float f) {
    uint u = asuint(f);
    return (u & 0x80000000u) != 0 ? ~u : u | 0x80000000u;
}

float unkey(uint k) {
    return asfloat((k & 0x80000000u) != 0 ? k & 0x7fffffffu : ~k);
}

[RootSignature(VIEW_RS)]
[numthreads(1, 1, 1)]
void reset() {
    range.Store2(0, uint2(0xffffffffu, 0));
}

// A group's lowest and highest key, before one lane takes them to `range`.
groupshared uint group_low;
groupshared uint group_high;

// The lowest and highest finite value of the channels shown, over a grid
// of the texture's texels.
[RootSignature(VIEW_RS)]
[numthreads(8, 8, 1)]
void reduce(uint3 id : SV_DispatchThreadID, uint lane : SV_GroupIndex) {
    if (lane == 0) {
        group_low = 0xffffffffu;
        group_high = 0;
    }
    GroupMemoryBarrierWithGroupSync();
    uint2 size = words[0].xy;
    uint2 at = id.xy * words[0].w;
    uint low = 0xffffffffu;
    uint high = 0;
    if (all(at < size)) {
        float4 v = fetch(int2(at));
        uint c = head.x;
        for (uint k = 0; k < 3; k++) {
            float x = c < 4 ? v[c] : v[k];
            if (isfinite(x)) {
                low = min(low, key(x));
                high = max(high, key(x));
            }
        }
    }
    uint was;
    InterlockedMin(group_low, low, was);
    InterlockedMax(group_high, high, was);
    GroupMemoryBarrierWithGroupSync();
    if (lane == 0) {
        range.InterlockedMin(0, group_low, was);
        range.InterlockedMax(4, group_high, was);
    }
}

[RootSignature(VIEW_RS)]
float4 view(float4 p : SV_Position) : SV_Target {
    uint2 size = words[0].xy;
    float2 uv = (p.xy - rect.xy) / (rect.zw - rect.xy);
    int2 at = int2(min(uint2(uv * float2(size)), size - 1));
    float4 v = fetch(at);
    uint2 keys = range.Load2(0);
    // No finite value at all: shown as it is.
    float low = 0;
    float high = 1;
    if (keys.x <= keys.y) {
        low = unkey(keys.x);
        high = unkey(keys.y);
    }
    uint c = head.x;
    uint show = head.y;
    float3 shown = c < 4 ? v[c].xxx : v.rgb;
    if ((show & SHOW_RANGE) != 0) {
        shown = (shown - low) / max(high - low, 1e-20);
    } else if ((show & SHOW_HIGHEST) != 0) {
        shown = shown / max(high, 1e-20);
    }
    shown = saturate(shown);
    if ((show & SHOW_CURVE) != 0) {
        shown = pow(shown, 1.0 / 3.0);
    }
    return float4(shown, 1);
}
