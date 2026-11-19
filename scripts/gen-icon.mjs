// duthub 应用图标生成器（纯 Node 实现：逐像素光栅化 + PNG 编码 + ICO 打包）
// 设计：青色渐变圆角方（品牌色）+ 深色 UART 波形（串口 0101 帧）+ 亮色终端光标块
// 用法: node scripts/gen-icon.mjs [outDir]
import { writeFileSync, mkdirSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { deflateSync } from "node:zlib";

const scriptDir = dirname(fileURLToPath(import.meta.url));
const outDir = process.argv[2] ?? join(scriptDir, "..", "apps", "desktop", "assets");
mkdirSync(outDir, { recursive: true });

// ---------- 几何定义（256 设计基准） ----------
const R = 8, RS = 240, RR = 56; // 背景圆角方
const INK = [7, 16, 24];        // 机器人主体色（深色）
const GLOW = [235, 255, 250];   // 眼睛发光色（近白）
const TEAL_A = [90, 220, 198];  // 渐变亮端
const TEAL_B = [36, 146, 128];  // 渐变暗端

// 机器人头（圆角矩形）
const HEAD = { x: 64, y: 92, w: 128, h: 96, r: 24 };
// 天线：顶部圆 + 茎
const ANT_TIP = { cx: 128, cy: 52, r: 11 };
const ANT_STEM_W = 10;
// 眼睛（青色发光圆）
const EYES = [
  { cx: 96, cy: 140, r: 15 },
  { cx: 160, cy: 140, r: 15 },
];
// 耳朵（两侧小块）
const EARS = [
  { x: 48, y: 124, w: 14, h: 36, r: 6 },
  { x: 194, y: 124, w: 14, h: 36, r: 6 },
];

function insideRoundedRect(x, y, rx, ry, rw, rh, rr) {
  if (x < rx || x >= rx + rw || y < ry || y >= ry + rh) return false;
  const cx = Math.max(rx + rr, Math.min(x, rx + rw - rr));
  const cy = Math.max(ry + rr, Math.min(y, ry + rh - rr));
  const dx = x - cx, dy = y - cy;
  return dx * dx + dy * dy <= rr * rr;
}

function distToRoundedRectEdge(x, y, rx, ry, rw, rh, rr) {
  const cx = Math.max(rx + rr, Math.min(x, rx + rw - rr));
  const cy = Math.max(ry + rr, Math.min(y, ry + rh - rr));
  return Math.hypot(x - cx, y - cy) - rr;
}

function insideCircle(x, y, cx, cy, r) {
  const dx = x - cx, dy = y - cy;
  return dx * dx + dy * dy <= r * r;
}

function distToCircleEdge(x, y, cx, cy, r) {
  return Math.hypot(x - cx, y - cy) - r;
}

function distToSegment(px, py, ax, ay, bx, by) {
  const dx = bx - ax, dy = by - ay;
  const t = Math.max(0, Math.min(1, ((px - ax) * dx + (py - ay) * dy) / (dx * dx + dy * dy || 1)));
  return Math.hypot(px - (ax + t * dx), py - (ay + t * dy));
}

function coverage(sample) {
  // 4x4 超采样
  let inside = 0, head = 0, antenna = 0, eyes = 0, ears = 0;
  for (let sy = 0; sy < 4; sy++) {
    for (let sx = 0; sx < 4; sx++) {
      const x = sample.x + (sx + 0.5) / 4;
      const y = sample.y + (sy + 0.5) / 4;
      const inBg = insideRoundedRect(x, y, R, R, RS, RS, RR);
      if (!inBg) continue;
      inside++;
      // 头部
      if (insideRoundedRect(x, y, HEAD.x, HEAD.y, HEAD.w, HEAD.h, HEAD.r)) head++;
      // 天线：茎（线段）+ 顶圆；同 subpixel 双命中只计一次（否则覆盖率 >1 产生负色）
      const stemHit =
        distToSegment(x, y, ANT_TIP.cx, ANT_TIP.cy, HEAD.x + HEAD.w / 2, HEAD.y) <=
        ANT_STEM_W / 2;
      const tipHit = insideCircle(x, y, ANT_TIP.cx, ANT_TIP.cy, ANT_TIP.r);
      if (stemHit || tipHit) antenna++;
      // 眼睛
      for (const e of EYES) {
        if (insideCircle(x, y, e.cx, e.cy, e.r)) eyes++;
      }
      // 耳朵
      for (const ear of EARS) {
        if (insideRoundedRect(x, y, ear.x, ear.y, ear.w, ear.h, ear.r)) ears++;
      }
    }
  }
  const total = 16;
  return { inside: inside / total, head: head / total, antenna: antenna / total, eyes: eyes / total, ears: ears / total };
}

function render(size) {
  const px = new Uint8Array(size * size * 4);
  const k = 256 / size; // 每个目标像素覆盖的设计坐标块
  for (let y = 0; y < size; y++) {
    for (let x = 0; x < size; x++) {
      const c = coverage({ x: x * k, y: y * k, k });
      const dbg = x === 128 && y === 52;
      if (dbg) console.error('cov(128,52):', JSON.stringify(c));
      if (c.inside <= 0) continue; // 透明（窗口圆角外）
      // 渐变 teal（对角线插值）
      const t = (x * k + y * k) / 512;
      let r = TEAL_A[0] + (TEAL_B[0] - TEAL_A[0]) * t;
      let g = TEAL_A[1] + (TEAL_B[1] - TEAL_A[1]) * t;
      let b = TEAL_A[2] + (TEAL_B[2] - TEAL_A[2]) * t;
      if (dbg) console.error('base:', r.toFixed(1), g.toFixed(1), b.toFixed(1));
      // 机器人头（深色）
      if (c.head > 0) {
        r = r * (1 - c.head) + INK[0] * c.head;
        g = g * (1 - c.head) + INK[1] * c.head;
        b = b * (1 - c.head) + INK[2] * c.head;
      }
      // 天线
      if (c.antenna > 0) {
        r = r * (1 - c.antenna) + INK[0] * c.antenna;
        g = g * (1 - c.antenna) + INK[1] * c.antenna;
        b = b * (1 - c.antenna) + INK[2] * c.antenna;
      }
      // 耳朵
      if (c.ears > 0) {
        r = r * (1 - c.ears) + INK[0] * c.ears;
        g = g * (1 - c.ears) + INK[1] * c.ears;
        b = b * (1 - c.ears) + INK[2] * c.ears;
      }
      if (dbg) console.error('after head/ant/ears:', r.toFixed(1), g.toFixed(1), b.toFixed(1), 'eyes=', c.eyes);
      // 眼睛（发光青白）
      if (c.eyes > 0) {
        r = r * (1 - c.eyes) + GLOW[0] * c.eyes;
        g = g * (1 - c.eyes) + GLOW[1] * c.eyes;
        b = b * (1 - c.eyes) + GLOW[2] * c.eyes;
      }
      const o = (y * size + x) * 4;
      const clamp = (v) => Math.max(0, Math.min(255, Math.round(v)));
      px[o] = clamp(r);
      px[o + 1] = clamp(g);
      px[o + 2] = clamp(b);
      px[o + 3] = clamp(255 * Math.min(1, c.inside * 1.15));
    }
  }
  const o = (52 * size + 128) * 4;
  if (size === 256) console.error('px(128,52):', px[o], px[o+1], px[o+2], px[o+3]);
  return px;
}

// ---------- PNG 编码（RGBA + filter 0） ----------
function crc32(buf) {
  let table = crc32.table;
  if (!table) {
    table = crc32.table = new Int32Array(256);
    for (let n = 0; n < 256; n++) {
      let c = n;
      for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
      table[n] = c;
    }
  }
  let c = -1;
  for (let i = 0; i < buf.length; i++) c = table[(c ^ buf[i]) & 0xff] ^ (c >>> 8);
  return (c ^ -1) >>> 0;
}

function chunk(type, data) {
  const out = Buffer.alloc(12 + data.length);
  out.writeUInt32BE(data.length, 0);
  out.write(type, 4, "ascii");
  data.copy(out, 8);
  out.writeUInt32BE(crc32(out.subarray(4, 8 + data.length)), 8 + data.length);
  return out;
}

function encodePNG(pxBuf, size) {
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(size, 0);
  ihdr.writeUInt32BE(size, 4);
  ihdr[8] = 8; // bit depth
  ihdr[9] = 6; // color type RGBA
  const raw = Buffer.alloc(size * (size * 4 + 1));
  for (let y = 0; y < size; y++) {
    raw[y * (size * 4 + 1)] = 0; // filter none
    const src = Buffer.from(pxBuf.buffer, y * size * 4, size * 4);
    src.copy(raw, y * (size * 4 + 1) + 1);
  }
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk("IHDR", ihdr),
    chunk("IDAT", deflateSync(raw, { level: 9 })),
    chunk("IEND", Buffer.alloc(0)),
  ]);
}

// ---------- 生成 ----------
const master256 = render(256);
writeFileSync(join(outDir, "app-256.png"), encodePNG(master256, 256));

const sizes = [256, 128, 64, 48, 32, 16];
const pngs = {};
for (const s of sizes) {
  if (s === 256) { pngs[s] = encodePNG(master256, 256); continue; }
  // area-average 缩小
  const k = 256 / s;
  const px = new Uint8Array(s * s * 4);
  for (let y = 0; y < s; y++) {
    for (let x = 0; x < s; x++) {
      let r = 0, g = 0, b = 0, a = 0, n = 0;
      for (let yy = Math.floor(y * k); yy < Math.ceil((y + 1) * k); yy++) {
        for (let xx = Math.floor(x * k); xx < Math.ceil((x + 1) * k); xx++) {
          const o = (yy * 256 + xx) * 4;
          r += master256[o]; g += master256[o + 1]; b += master256[o + 2]; a += master256[o + 3]; n++;
        }
      }
      const o = (y * s + x) * 4;
      px[o] = Math.round(r / n); px[o + 1] = Math.round(g / n);
      px[o + 2] = Math.round(b / n); px[o + 3] = Math.round(a / n);
    }
  }
  pngs[s] = encodePNG(px, s);
}

// ICO 打包
const header = Buffer.alloc(6);
header.writeUInt16LE(0, 0); header.writeUInt16LE(1, 2); header.writeUInt16LE(sizes.length, 4);
const entries = [];
let offset = 6 + 16 * sizes.length;
for (const s of sizes) {
  const e = Buffer.alloc(16);
  const dim = s >= 256 ? 0 : s;
  e[0] = dim; e[1] = dim; e[2] = 0; e[3] = 0;
  e.writeUInt16LE(1, 4); e.writeUInt16LE(32, 6);
  e.writeUInt32LE(pngs[s].length, 8);
  e.writeUInt32LE(offset, 12);
  entries.push(e);
  offset += pngs[s].length;
}
const ico = Buffer.concat([header, ...entries, ...sizes.map((s) => pngs[s])]);
writeFileSync(join(outDir, "app.ico"), ico);

console.log(`OK -> ${outDir} (app.ico ${ico.length}B, app-256.png)`);
