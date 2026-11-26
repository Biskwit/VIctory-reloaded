// What the host draws over the presented frame (`crate::overlay`): a panel
// of filled rectangles and text from a font's atlas. Compiled entry point by
// entry point with D3DCompile (shader model 5.1) when the overlay first
// draws, as `crate::compose`'s shaders are; nothing compiled is kept in the
// repository.
//
// The panel's root signature: the draw's 44 constants and the font's atlas
// as a root view.
#define PANEL_RS "RootConstants(num32BitConstants = 44, b0), SRV(t0)"

cbuffer Draw : register(b0) {
    // The rectangle drawn, in the target's pixels: left, top, right, bottom.
    float4 rect;
    // The target's size in pixels.
    float2 target;
    // The colour of the text or of a filled rectangle (8 bits a channel, red
    // lowest, alpha highest), and the cell of the text's face in pixels:
    // width in bits 0-7, height in bits 8-15 (0: filled).
    uint2 style;
    // The text's characters, and the byte of the atlas where its face
    // starts.
    uint4 head;
    // The characters, four a word, the first in the low byte.
    uint4 words[8];
};

// The font's atlas: faces of characters 32 to 126, a byte of coverage a
// pixel, glyph after glyph, each row after row (`crate::overlay::atlas`).
ByteAddressBuffer font : register(t0);

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
