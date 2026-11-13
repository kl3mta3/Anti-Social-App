// Draws the app icon (a dark badge with a ring and three role dots) and writes it as a PNG.
// `npm run icon` then turns it into every size Tauri needs.

import { mkdirSync, writeFileSync } from 'node:fs';
import { deflateSync } from 'node:zlib';

const SIZE = 1024;
const SS = 3; // samples per axis for smooth edges

const BG = [22, 26, 36];
const RING = [110, 168, 254];
const DOTS = [
  { x: 0.5, y: 0.33, c: [110, 168, 254] }, // tank
  { x: 0.345, y: 0.6, c: [76, 195, 138] }, // healer
  { x: 0.655, y: 0.6, c: [242, 119, 122] }, // dps
];

function sample(u, v) {
  const dx = u - 0.5;
  const dy = v - 0.5;
  const d = Math.hypot(dx, dy);
  if (d > 0.48) return null; // outside the badge: transparent
  if (d > 0.42) return RING;
  for (const dot of DOTS) {
    if (Math.hypot(u - dot.x, v - dot.y) < 0.105) return dot.c;
  }
  return BG;
}

const raw = Buffer.alloc(SIZE * (SIZE * 4 + 1));
for (let y = 0; y < SIZE; y++) {
  const row = y * (SIZE * 4 + 1);
  raw[row] = 0; // filter: none
  for (let x = 0; x < SIZE; x++) {
    let r = 0, g = 0, b = 0, a = 0;
    for (let sy = 0; sy < SS; sy++) {
      for (let sx = 0; sx < SS; sx++) {
        const c = sample((x + (sx + 0.5) / SS) / SIZE, (y + (sy + 0.5) / SS) / SIZE);
        if (c) {
          r += c[0];
          g += c[1];
          b += c[2];
          a += 1;
        }
      }
    }
    const o = row + 1 + x * 4;
    if (a > 0) {
      raw[o] = Math.round(r / a);
      raw[o + 1] = Math.round(g / a);
      raw[o + 2] = Math.round(b / a);
      raw[o + 3] = Math.round((a / (SS * SS)) * 255);
    }
  }
}

const crcTable = new Uint32Array(256).map((_, n) => {
  let c = n;
  for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
  return c >>> 0;
});

function crc32(buf) {
  let c = 0xffffffff;
  for (const byte of buf) c = crcTable[(c ^ byte) & 0xff] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
}

function chunk(type, data) {
  const len = Buffer.alloc(4);
  len.writeUInt32BE(data.length);
  const body = Buffer.concat([Buffer.from(type, 'ascii'), data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(body));
  return Buffer.concat([len, body, crc]);
}

const header = Buffer.alloc(13);
header.writeUInt32BE(SIZE, 0);
header.writeUInt32BE(SIZE, 4);
header[8] = 8; // bit depth
header[9] = 6; // RGBA
const png = Buffer.concat([
  Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
  chunk('IHDR', header),
  chunk('IDAT', deflateSync(raw, { level: 9 })),
  chunk('IEND', Buffer.alloc(0)),
]);

const out = new URL('../src-tauri/icons/', import.meta.url);
mkdirSync(out, { recursive: true });
writeFileSync(new URL('source.png', out), png);
console.log(`wrote ${png.length} bytes to src-tauri/icons/source.png`);
