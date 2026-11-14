#!/usr/bin/env node
// Decodes the matrices written by `qr_dump_matrices_for_external_check`
// (cargo test -p ant-cli qr) using the independent jsQR decoder, so a
// scanner-side regression cannot hide behind unit tests that share the
// encoder's own assumptions.
//
// Usage: node scripts/qr/verify-qr.js
// Requires the jsQR package (npm i jsqr) somewhere on the resolution path.
'use strict';

const fs = require('fs');
const os = require('os');
const path = require('path');

let jsQR;
try {
  jsQR = require('jsqr');
} catch (e) {
  try {
    jsQR = require(path.join(process.cwd(), 'node_modules', 'jsqr'));
  } catch (e2) {
    console.error('jsQR is not installed. Run: npm i jsqr');
    process.exit(2);
  }
}

const dir = path.join(os.tmpdir(), 'ant-qr-check');
if (!fs.existsSync(dir)) {
  console.error(`No matrices found in ${dir}. Run: cargo test -p ant-cli qr`);
  process.exit(2);
}

// Renders one matrix (one '#'/'.'
// per module) into an RGBA buffer at 8 px per module with a 4-module
// quiet zone — the geometry scanners expect.
function raster(rows, size) {
  const scale = 8;
  const quiet = 4; // modules
  const dim = (size + 2 * quiet) * scale;
  const data = new Uint8ClampedArray(dim * dim * 4).fill(255);
  rows.forEach((row, y) => {
    [...row].forEach((ch, x) => {
      if (ch !== '#') return;
      const px = (quiet + x) * scale;
      const py = (quiet + y) * scale;
      for (let dy = 0; dy < scale; dy++) {
        for (let dx = 0; dx < scale; dx++) {
          const p = ((py + dy) * dim + (px + dx)) * 4;
          data[p] = 0;
          data[p + 1] = 0;
          data[p + 2] = 0;
          data[p + 3] = 255;
        }
      }
    });
  });
  return { data, dim };
}

let failures = 0;
let total = 0;
for (const f of fs.readdirSync(dir).filter((f) => f.endsWith('.txt')).sort()) {
  const lines = fs
    .readFileSync(path.join(dir, f), 'utf8')
    .trim()
    .split('\n');
  const expected = lines[0].replace(/^# /, '');
  const size = parseInt(lines[1], 10);
  const rows = lines.slice(2);
  if (rows.length !== size || rows.some((r) => r.length !== size)) {
    console.log(`${f}: MALFORMED matrix (not ${size}x${size})`);
    failures++;
    total++;
    continue;
  }
  const { data, dim } = raster(rows, size);
  const result = jsQR(data, dim, dim);
  total++;
  if (result && result.data === expected) {
    console.log(`${f}: OK`);
  } else {
    console.log(
      `${f}: ${result ? 'WRONG ' + JSON.stringify(result.data) : 'FAILED'}` +
        ` (expected ${JSON.stringify(expected)})`,
    );
    failures++;
  }
}
console.log(`${total} checked, ${failures} failures`);
process.exit(failures ? 1 : 0);
