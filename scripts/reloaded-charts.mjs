// The README's charts (docs/charts/reloaded-*-{light,dark}.svg), from the numbers below:
// Series S, the prologue's cell, i9-12900K with an RTX 4090, scripts\play.ps1's 4090
// settings, warm launches, medians of the frame lines once the scene has settled.
//   node scripts/reloaded-charts.mjs
import { writeFile } from 'node:fs/promises';

// Each round's quiet-machine measurement (docs/optimizations.md, Measurement history).
const rounds = [
  { name: 'Vice-Tay', note: 'where we started', fps: 17.4, ms: 57.5, prep: 45.5, card: 43.8, copy: 24.1 },
  { name: 'Round 1', note: 'card copies, threads', fps: 19.9, ms: 50.3, prep: 40.0, card: 30.8, copy: 26.5 },
  { name: 'Round 2', note: 'ReBAR, split, guards', fps: 27.0, ms: 37.1, prep: 30.1, card: 20.3, copy: 18.2 },
  { name: 'Round 3', note: 'masks, sender', fps: 28.8, ms: 34.7, prep: 25.1, card: 20.2, copy: 17.4 },
];

// What each change took off the frame when it went in: the same run switching it on and
// off, mean of its launches, ms a frame. Changes measured otherwise (the card's time for the
// 64 KiB card copies and the windows in card memory, separate launches for the worker
// threads) are left out: the README's tables have them.
const levers = [
  { name: "FSR 4's scratch kept on the card", ms: 3.1 },
  { name: 'Submissions prepared while the last is sent', ms: 2.6 },
  { name: "Our threads off the title's processors", ms: 2.3 },
  { name: 'Card steps written by the sending thread', ms: 1.7 },
  { name: 'Words under shaders’ writes sent by mask', ms: 1.65 },
  { name: 'Only nearby card copies looked at', ms: 0.85 },
  { name: 'The sending thread presents', ms: 0.8 },
  { name: "A submission's steps walked once", ms: 0.575 },
  { name: 'Preludes recorded by the sending thread', ms: 0.525 },
  { name: 'Unchanged refreshes skipped by stamp', ms: 0.5 },
  { name: 'Copies under flights from the one walk', ms: 0.375 },
];

// The 12-minute session of round 3: fps a minute (1000 / the minute's median ms), from its
// second minute, the first whole one in the cell.
const sessionFirst = 2;
const session = [34.6, 34.1, 33.3, 33.4, 34.1, 34.3, 34.3, 33.8, 33.3, 33.3].map(ms => 1000 / ms);

const themes = {
  light: {
    surface: '#fcfcfb', ink: '#0b0b0b', ink2: '#52514e', muted: '#898781', grid: '#e1e0d9',
    axis: '#c3c2b7', accent: '#2a78d6', before: '#86b6ef', after: '#2a78d6', border: 'rgba(11,11,11,0.10)',
  },
  dark: {
    surface: '#1a1a19', ink: '#f0efec', ink2: '#c3c2b7', muted: '#898781', grid: '#2c2c2a',
    axis: '#383835', accent: '#3987e5', before: '#184f95', after: '#5598e7', border: 'rgba(255,255,255,0.10)',
  },
};

const width = 920;
const esc = s => String(s).replace(/[&<>"']/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&apos;' })[c]);
const f1 = v => v.toFixed(1);

function svg(t, title, subtitle, height, body) {
  return `<svg xmlns="http://www.w3.org/2000/svg" width="${width}" height="${height}" viewBox="0 0 ${width} ${height}" role="img" aria-labelledby="t d">
<title id="t">${esc(title)}</title>
<desc id="d">${esc(subtitle)}</desc>
<style>text{font-family:system-ui,-apple-system,'Segoe UI',sans-serif}.tick{font-variant-numeric:tabular-nums}</style>
<rect x="0.5" y="0.5" width="${width - 1}" height="${height - 1}" rx="12" fill="${t.surface}" stroke="${t.border}"/>
<text x="32" y="44" fill="${t.ink}" font-size="20" font-weight="600">${esc(title)}</text>
<text x="32" y="68" fill="${t.ink2}" font-size="14">${esc(subtitle)}</text>
${body.join('\n')}
</svg>
`;
}

// A column from the baseline, its top end rounded (4px), its foot square.
function column(x, top, w, base, fill) {
  const r = Math.min(4, (base - top) / 2);
  return `<path d="M${x} ${base}V${top + r}Q${x} ${top} ${x + r} ${top}H${x + w - r}Q${x + w} ${top} ${x + w} ${top + r}V${base}Z" fill="${fill}"/>`;
}

// A bar from the left baseline, its right end rounded (4px).
function bar(left, y, len, h, fill) {
  const r = Math.min(4, len / 2);
  return `<path d="M${left} ${y}H${left + len - r}Q${left + len} ${y} ${left + len} ${y + r}V${y + h - r}Q${left + len} ${y + h} ${left + len - r} ${y + h}H${left}Z" fill="${fill}"/>`;
}

function fpsChart(t) {
  const height = 420, top = 100, base = 340, left = 72, right = width - 40;
  const max = 35, y = v => base - (v / max) * (base - top);
  const body = [];
  for (const v of [0, 10, 20, 30]) {
    body.push(`<line x1="${left}" x2="${right}" y1="${y(v)}" y2="${y(v)}" stroke="${v ? t.grid : t.axis}" stroke-width="1"/>`);
    body.push(`<text class="tick" x="${left - 12}" y="${y(v) + 5}" text-anchor="end" fill="${t.muted}" font-size="13">${v}</text>`);
  }
  body.push(`<line x1="${left}" x2="${right}" y1="${y(30)}" y2="${y(30)}" stroke="${t.ink2}" stroke-width="1.5"/>`);
  body.push(`<text x="${right}" y="${y(30) - 8}" text-anchor="end" fill="${t.ink2}" font-size="13">30 fps</text>`);
  const slot = (right - left) / rounds.length, w = 24;
  rounds.forEach((r, i) => {
    const cx = left + slot * (i + 0.5);
    body.push(column(cx - w / 2, y(r.fps), w, base, t.accent));
    body.push(`<text x="${cx}" y="${y(r.fps) - 10}" text-anchor="middle" fill="${t.ink}" font-size="15" font-weight="600">${f1(r.fps)} fps</text>`);
    body.push(`<text x="${cx}" y="${base + 24}" text-anchor="middle" fill="${t.ink}" font-size="14">${esc(r.name)}</text>`);
    body.push(`<text x="${cx}" y="${base + 42}" text-anchor="middle" fill="${t.muted}" font-size="12">${esc(r.note)}</text>`);
  });
  return svg(t, 'Series S frame rate, round by round', 'Prologue cell, i9-12900K + RTX 4090, warm launches, median of the settled frames', height, body);
}

function partsChart(t) {
  const first = rounds[0], now = rounds[rounds.length - 1];
  const rows = [
    ['Frame', first.ms, now.ms],
    ['Preparing (CPU)', first.prep, now.prep],
    ['Card busy (GPU)', first.card, now.card],
    ['Copying back (CPU)', first.copy, now.copy],
  ];
  const height = 384, top = 132, rowH = 48, left = 200, right = width - 48, max = 60;
  const x = v => left + (v / max) * (right - left);
  const base = top + rows.length * rowH;
  const body = [];
  // Legend, above the plot.
  body.push(`<circle cx="40" cy="98" r="5" fill="${t.before}" stroke="${t.surface}" stroke-width="2"/>`);
  body.push(`<text x="52" y="103" fill="${t.ink2}" font-size="13">${esc(`${first.name} (${first.note})`)}</text>`);
  body.push(`<circle cx="262" cy="98" r="5" fill="${t.after}" stroke="${t.surface}" stroke-width="2"/>`);
  body.push(`<text x="274" y="103" fill="${t.ink2}" font-size="13">${esc(`${now.name} (${now.note})`)}</text>`);
  for (const v of [0, 10, 20, 30, 40, 50, 60]) {
    body.push(`<line x1="${x(v)}" x2="${x(v)}" y1="${top - 8}" y2="${base}" stroke="${v ? t.grid : t.axis}" stroke-width="1"/>`);
    body.push(`<text class="tick" x="${x(v)}" y="${base + 20}" text-anchor="middle" fill="${t.muted}" font-size="13">${v}</text>`);
  }
  body.push(`<text x="${right}" y="${base + 40}" text-anchor="end" fill="${t.muted}" font-size="12">ms a frame</text>`);
  rows.forEach(([name, a, b], i) => {
    const cy = top + rowH * (i + 0.5);
    body.push(`<text x="${left - 16}" y="${cy + 5}" text-anchor="end" fill="${t.ink}" font-size="14">${esc(name)}</text>`);
    body.push(`<line x1="${x(b)}" x2="${x(a)}" y1="${cy}" y2="${cy}" stroke="${t.axis}" stroke-width="2" stroke-linecap="round"/>`);
    body.push(`<circle cx="${x(a)}" cy="${cy}" r="6" fill="${t.before}" stroke="${t.surface}" stroke-width="2"/>`);
    body.push(`<circle cx="${x(b)}" cy="${cy}" r="6" fill="${t.after}" stroke="${t.surface}" stroke-width="2"/>`);
    body.push(`<text class="tick" x="${x(a) + 12}" y="${cy + 5}" fill="${t.ink2}" font-size="13">${f1(a)}</text>`);
    body.push(`<text class="tick" x="${x(b) - 12}" y="${cy + 5}" text-anchor="end" fill="${t.ink}" font-size="13" font-weight="600">${f1(b)}</text>`);
  });
  return svg(t, 'Where a frame goes', 'Series S, prologue cell: each part of the frame where we started and now (the parts overlap in time)', height, body);
}

function leverChart(t) {
  const rowH = 32, top = 100, left = 360, right = width - 90, max = 3.5;
  const x = v => left + (v / max) * (right - left);
  const height = top + levers.length * rowH + 56;
  const base = top + levers.length * rowH;
  const body = [];
  for (const v of [0, 1, 2, 3]) {
    body.push(`<line x1="${x(v)}" x2="${x(v)}" y1="${top - 6}" y2="${base}" stroke="${v ? t.grid : t.axis}" stroke-width="1"/>`);
    body.push(`<text class="tick" x="${x(v)}" y="${base + 20}" text-anchor="middle" fill="${t.muted}" font-size="13">${v}</text>`);
  }
  body.push(`<text x="${x(max)}" y="${base + 40}" text-anchor="end" fill="${t.muted}" font-size="12">ms a frame taken off</text>`);
  [...levers].sort((a, b) => b.ms - a.ms).forEach((l, i) => {
    const y = top + i * rowH, h = 16, cy = y + rowH / 2;
    body.push(`<text x="${left - 14}" y="${cy + 5}" text-anchor="end" fill="${t.ink}" font-size="13.5">${esc(l.name)}</text>`);
    body.push(bar(left, cy - h / 2, x(l.ms) - left, h, t.accent));
    body.push(`<text class="tick" x="${x(l.ms) + 8}" y="${cy + 5}" fill="${t.ink2}" font-size="13">${l.ms.toFixed(l.ms < 1 ? 2 : 1)}</text>`);
  });
  return svg(t, 'What each change took off the frame', 'Series S, prologue cell: the same run switching the change on and off, when it went in', height, body);
}

function sessionChart(t) {
  const height = 380, top = 100, base = 320, left = 72, right = width - 48, max = 35;
  const n = session.length;
  const x = i => left + (i / Math.max(1, n - 1)) * (right - left);
  const y = v => base - (v / max) * (base - top);
  const body = [];
  for (const v of [0, 10, 20, 30]) {
    body.push(`<line x1="${left}" x2="${right}" y1="${y(v)}" y2="${y(v)}" stroke="${v ? t.grid : t.axis}" stroke-width="1"/>`);
    body.push(`<text class="tick" x="${left - 12}" y="${y(v) + 5}" text-anchor="end" fill="${t.muted}" font-size="13">${v}</text>`);
  }
  body.push(`<line x1="${left}" x2="${right}" y1="${y(30)}" y2="${y(30)}" stroke="${t.ink2}" stroke-width="1.5"/>`);
  body.push(`<text x="${right}" y="${y(30) - 8}" text-anchor="end" fill="${t.ink2}" font-size="13">30 fps</text>`);
  const pts = session.map((v, i) => `${x(i).toFixed(1)},${y(v).toFixed(1)}`).join(' ');
  body.push(`<polygon points="${x(0)},${base} ${pts} ${x(n - 1)},${base}" fill="${t.accent}" fill-opacity="0.10"/>`);
  body.push(`<polyline points="${pts}" fill="none" stroke="${t.accent}" stroke-width="2" stroke-linejoin="round" stroke-linecap="round"/>`);
  session.forEach((v, i) => body.push(`<text class="tick" x="${x(i)}" y="${base + 20}" text-anchor="middle" fill="${t.muted}" font-size="13">${i + sessionFirst}</text>`));
  body.push(`<text x="${right}" y="${base + 40}" text-anchor="end" fill="${t.muted}" font-size="12">minute of the session</text>`);
  const last = session[n - 1];
  body.push(`<circle cx="${x(n - 1)}" cy="${y(last)}" r="5" fill="${t.accent}" stroke="${t.surface}" stroke-width="2"/>`);
  body.push(`<text x="${x(n - 1) - 10}" y="${y(last) + 24}" text-anchor="end" fill="${t.ink}" font-size="14" font-weight="600">${f1(last)} fps</text>`);
  return svg(t, 'A 12-minute session, minute by minute', 'Series S, prologue cell, round 3: median fps of each minute in the cell (the first went to loading)', height, body);
}

for (const [mode, t] of Object.entries(themes)) {
  await writeFile(`docs/charts/reloaded-fps-${mode}.svg`, fpsChart(t));
  await writeFile(`docs/charts/reloaded-frame-parts-${mode}.svg`, partsChart(t));
  await writeFile(`docs/charts/reloaded-levers-${mode}.svg`, leverChart(t));
  if (session.length) await writeFile(`docs/charts/reloaded-session-${mode}.svg`, sessionChart(t));
}
