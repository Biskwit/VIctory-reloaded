// The amplification stage of a title mesh pipeline (`crate::pipeline`):
// launches the title's mesh groups for a draw, so that an indirect draw of
// one record needs no pass of ours before it. Compiled with the Windows
// SDK's compiler into amplify.dxil, which the crate embeds:
//   dxc -T as_6_5 -E main -Fo amplify.dxil amplify.hlsl
//
// `direct` nonzero: a direct mesh dispatch of that many groups. Else the
// record (three group counts) where the title keeps it is bounded as the
// argument clamp bounds a mesh record (`crate::indirect`), and charged to
// the list's budget in the same words.
cbuffer Draw : register(b0, space900) {
    uint direct;
};
ByteAddressBuffer record : register(t0, space900);
RWByteAddressBuffer budget : register(u0, space900);

static const uint BUDGET = 1u << 27;

struct Payload {
    uint unused;
};
groupshared Payload payload;

[numthreads(1, 1, 1)]
void main() {
    uint groups = min(direct, 65535u);
    if (direct == 0u) {
        uint3 g = record.Load3(0);
        uint was;
        budget.InterlockedMax(4, g.x, was);
        budget.InterlockedAdd(8, min(g.x, 1u << 24), was);
        // The console's mesh dispatches are one-dimensional.
        groups = min(g.x, 65535u) * min(g.y, 1u) * min(g.z, 1u);
        uint cost = groups * 128u;
        if (cost > 0u) {
            uint before;
            budget.InterlockedOr(0, 0, before);
            [allow_uav_condition]
            while (before < BUDGET) {
                uint next = (BUDGET - before <= cost) ? BUDGET : (before + cost);
                uint prev;
                budget.InterlockedCompareExchange(0, before, next, prev);
                if (prev == before) {
                    break;
                }
                before = prev;
            }
            if (before >= BUDGET || BUDGET - before < cost) {
                groups = 0u;
            }
        }
    }
    payload.unused = 0u;
    DispatchMesh(groups, 1, 1, payload);
}
