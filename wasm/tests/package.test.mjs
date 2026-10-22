// Tests of the built npm package (wasm/pkg, or $LIBIMGQUANT_PKG): `node --test wasm/tests/`.
// $LIBIMGQUANT_TEST_PNG names a real PNG file to compress as well; it is copied into a
// temporary directory first.

import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { basename, join, resolve } from "node:path";
import { after, describe, test } from "node:test";
import { fileURLToPath, pathToFileURL } from "node:url";
import { deflateSync, inflateSync } from "node:zlib";

const pkgDir = resolve(process.env.LIBIMGQUANT_PKG ?? fileURLToPath(new URL("../pkg/", import.meta.url)));
const { compressPng, quantize, quantizeFrames } = await import(pathToFileURL(join(pkgDir, "node.js")).href);

const temp = mkdtempSync(join(tmpdir(), "libimgquant-test-"));
after(() => rmSync(temp, { recursive: true, force: true }));

/** A gradient with a translucent band at the top, plus some deterministic noise. */
function gradient(width, height, blue = 90) {
  const rgba = new Uint8ClampedArray(width * height * 4);
  let seed = 1;
  for (let y = 0; y < height; y++) {
    for (let x = 0; x < width; x++) {
      seed = (seed * 1103515245 + 12345) >>> 0;
      const noise = (seed >>> 28) - 8;
      const i = (y * width + x) * 4;
      rgba[i] = (x * 255) / width + noise;
      rgba[i + 1] = (y * 255) / height + noise;
      rgba[i + 2] = blue;
      rgba[i + 3] = y < height / 8 ? 120 : 255;
    }
  }
  return rgba;
}

/** One color everywhere. */
function solid(width, height, [r, g, b, a]) {
  const rgba = new Uint8Array(width * height * 4);
  for (let i = 0; i < rgba.length; i += 4) rgba.set([r, g, b, a], i);
  return rgba;
}

const CRC_TABLE = Array.from({ length: 256 }, (_, n) => {
  let c = n;
  for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
  return c >>> 0;
});

function crc32(bytes) {
  let crc = ~0;
  for (const b of bytes) crc = CRC_TABLE[(crc ^ b) & 0xff] ^ (crc >>> 8);
  return ~crc >>> 0;
}

/** A truecolor-with-alpha PNG, as an image editor would save it. */
function rgbaPng(rgba, width, height) {
  const chunk = (type, data) => {
    const body = Buffer.concat([Buffer.from(type, "latin1"), data]);
    const out = Buffer.alloc(body.length + 8);
    out.writeUInt32BE(data.length, 0);
    body.copy(out, 4);
    out.writeUInt32BE(crc32(body), body.length + 4);
    return out;
  };
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(width, 0);
  ihdr.writeUInt32BE(height, 4);
  ihdr.set([8, 6, 0, 0, 0], 8); // 8 bits, RGBA
  const raw = Buffer.alloc(height * (1 + width * 4));
  for (let y = 0; y < height; y++) {
    raw[y * (1 + width * 4)] = 0; // filter None
    raw.set(rgba.subarray(y * width * 4, (y + 1) * width * 4), y * (1 + width * 4) + 1);
  }
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk("IHDR", ihdr),
    chunk("IDAT", deflateSync(raw, { level: 9 })),
    chunk("IEND", Buffer.alloc(0)),
  ]);
}

/** The chunks of a PNG file, checking the signature and every CRC. */
function chunks(png) {
  const bytes = Buffer.from(png);
  assert.deepEqual([...bytes.subarray(0, 8)], [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a], "PNG signature");
  const out = [];
  for (let at = 8; at < bytes.length; ) {
    const length = bytes.readUInt32BE(at);
    const type = bytes.toString("latin1", at + 4, at + 8);
    const data = bytes.subarray(at + 8, at + 8 + length);
    assert.equal(bytes.readUInt32BE(at + 8 + length), crc32(bytes.subarray(at + 4, at + 8 + length)), `${type} CRC`);
    out.push({ type, data });
    at += 12 + length;
  }
  return out;
}

/** Checks that `png` is a valid palette PNG of the given size; returns its palette size. */
function checkIndexedPng(png, width, height) {
  assert.ok(png instanceof Uint8Array);
  assert.equal(png[25], 3, "IHDR color type 3 (indexed)");
  const list = chunks(png);
  assert.equal(list[0].type, "IHDR");
  assert.equal(list.at(-1).type, "IEND");
  const ihdr = list[0].data;
  assert.equal(ihdr.readUInt32BE(0), width);
  assert.equal(ihdr.readUInt32BE(4), height);
  const depth = ihdr[8];
  assert.ok([1, 2, 4, 8].includes(depth));
  const plte = list.find((c) => c.type === "PLTE");
  assert.ok(plte && plte.data.length % 3 === 0 && plte.data.length / 3 <= 2 ** depth);
  const idat = Buffer.concat(list.filter((c) => c.type === "IDAT").map((c) => c.data));
  const raw = inflateSync(idat);
  assert.equal(raw.length, height * (1 + Math.ceil((width * depth) / 8)), "decompressed size");
  return plte.data.length / 3;
}

describe("quantize", () => {
  const [width, height] = [64, 48];
  const pixels = gradient(width, height);

  test("returns a palette, one index per pixel and the quality", () => {
    const result = quantize(pixels, width, height, { maxColors: 32 });
    assert.ok(result.palette instanceof Uint8Array);
    assert.ok(result.indices instanceof Uint8Array);
    const colors = result.palette.length / 4;
    assert.ok(Number.isInteger(colors) && colors >= 2 && colors <= 32, `${colors} colors`);
    assert.equal(result.indices.length, width * height);
    assert.ok(result.indices.every((i) => i < colors));
    assert.ok(result.palette[3] < 255, "translucent entries come first");
    assert.equal(typeof result.quality, "number");
    assert.ok(result.quality >= 0 && result.quality <= 100);
  });

  test("the remapped image is close to the original", () => {
    const { palette, indices } = quantize(pixels, width, height);
    let error = 0;
    for (let p = 0; p < indices.length; p++) {
      for (let c = 0; c < 4; c++) error += Math.abs(palette[indices[p] * 4 + c] - pixels[p * 4 + c]);
    }
    assert.ok(error / pixels.length < 4, `mean channel error ${error / pixels.length}`);
  });

  test("accepts Uint8Array, Uint8ClampedArray, Buffer and ArrayBuffer alike", () => {
    const expected = quantize(pixels, width, height);
    for (const input of [new Uint8Array(pixels), Buffer.from(pixels), pixels.slice().buffer]) {
      assert.deepEqual(quantize(input, width, height), expected);
    }
  });

  test("options change the result", () => {
    const base = quantize(pixels, width, height);
    assert.equal(quantize(pixels, width, height, { maxColors: 8 }).palette.length, 8 * 4);
    assert.notDeepEqual(quantize(pixels, width, height, { dithering: 0 }).indices, base.indices);
    assert.deepEqual(quantize(pixels, width, height, { dithering: 0 }), quantize(pixels, width, height, { dithering: false }));
    assert.notDeepEqual(quantize(pixels, width, height, { profile: "compat" }), base);
    assert.notDeepEqual(quantize(pixels, width, height, { speed: 10 }), base);
    // the two quality forms are the same
    assert.deepEqual(
      quantize(pixels, width, height, { quality: [0, 70] }),
      quantize(pixels, width, height, { quality: { min: 0, max: 70 } }),
    );
    // a lower target needs fewer colors
    assert.ok(quantize(pixels, width, height, { quality: { max: 50 } }).palette.length < base.palette.length);
    // null and undefined mean the defaults
    assert.deepEqual(quantize(pixels, width, height, null), base);
    assert.deepEqual(quantize(pixels, width, height, { speed: undefined }), base);
  });
});

describe("compressPng", () => {
  test("writes a smaller palette PNG of the same size", () => {
    const [width, height] = [96, 64];
    const input = rgbaPng(gradient(width, height), width, height);
    const output = compressPng(input);
    const colors = checkIndexedPng(output, width, height);
    assert.ok(colors <= 256);
    assert.ok(output.length < input.length, `${output.length} bytes from ${input.length}`);
    const small = compressPng(input.buffer.slice(input.byteOffset, input.byteOffset + input.length), {
      maxColors: 4,
      dithering: 0,
    });
    assert.ok(checkIndexedPng(small, width, height) <= 4);
    assert.ok(small.length < output.length);
  });

  test("compresses a real PNG file ($LIBIMGQUANT_TEST_PNG)", { skip: !process.env.LIBIMGQUANT_TEST_PNG }, () => {
    const source = process.env.LIBIMGQUANT_TEST_PNG;
    const copy = join(temp, basename(source));
    copyFileSync(source, copy);
    const input = readFileSync(copy);
    const width = input.readUInt32BE(16);
    const height = input.readUInt32BE(20);
    const output = compressPng(input, { quality: [0, 90] });
    checkIndexedPng(output, width, height);
    assert.ok(output.length < input.length, `${output.length} bytes from ${input.length}`);
    writeFileSync(join(temp, "out.png"), output);
  });
});

describe("quantizeFrames", () => {
  test("quantizes all frames to one palette", () => {
    const [width, height] = [32, 24];
    const red = solid(width, height, [220, 20, 30, 255]);
    const blue = solid(width, height, [10, 40, 230, 255]);
    const result = quantizeFrames([red, gradient(width, height), blue], width, height, { maxColors: 64 });
    assert.ok(result.palette instanceof Uint8Array);
    assert.equal(result.frames.length, 3);
    const colors = result.palette.length / 4;
    for (const frame of result.frames) {
      assert.ok(frame instanceof Uint8Array);
      assert.equal(frame.length, width * height);
      assert.ok(frame.every((i) => i < colors));
    }
    // the colors of the single-color frames are in the shared palette exactly
    const color = (i) => [...result.palette.subarray(i * 4, i * 4 + 4)];
    assert.deepEqual(color(result.frames[0][0]), [220, 20, 30, 255]);
    assert.deepEqual(color(result.frames[2][0]), [10, 40, 230, 255]);
  });

  test("one frame gives the same palette size as quantize", () => {
    const pixels = gradient(40, 30);
    const frames = quantizeFrames([pixels], 40, 30, { maxColors: 16 });
    assert.equal(frames.palette.length, 16 * 4);
    assert.equal(frames.frames[0].length, 40 * 30);
  });
});

describe("errors", () => {
  const pixels = gradient(16, 16);

  test("are thrown as Error with a clear message", () => {
    const throws = (fn, message) => assert.throws(fn, (e) => e instanceof Error && message.test(e.message));
    throws(() => quantize(pixels, 16, 17), /16x17 RGBA pixels need 1088/);
    throws(() => quantize(pixels, 0, 16), /width and height must be positive integers/);
    throws(() => quantize(pixels, 15.5, 16), /positive integers/);
    throws(() => quantize([1, 2, 3, 4], 1, 1), /rgba must be a Uint8Array/);
    throws(() => quantize(pixels, 16, 16, { colors: 16 }), /unknown option 'colors'/);
    throws(() => quantize(pixels, 16, 16, { maxColors: 300 }), /maxColors must be an integer from 2 to 256, not 300/);
    throws(() => quantize(pixels, 16, 16, { speed: 0 }), /speed must be an integer from 1 to 10/);
    throws(() => quantize(pixels, 16, 16, { dithering: 2 }), /dithering must be a number from 0 to 1/);
    throws(() => quantize(pixels, 16, 16, { profile: "fast" }), /profile must be 'improved' or 'compat'/);
    throws(() => quantize(pixels, 16, 16, { quality: [90, 60] }), /quality min 90 is above max 60/);
    throws(() => quantize(pixels, 16, 16, { quality: "65-90" }), /quality must be \{min, max\} or \[min, max\]/);
    throws(() => quantize(pixels, 16, 16, "fast"), /options must be an object/);
    throws(
      () => quantize(gradient(64, 64), 64, 64, { maxColors: 2, quality: [90, 100] }),
      /quality too low: the image can't reach the minimum quality 90 with 2 colors/,
    );
    throws(() => compressPng(new Uint8Array([1, 2, 3])), /invalid PNG/);
    throws(() => compressPng("file.png"), /png must be a Uint8Array/);
    throws(() => quantizeFrames([], 16, 16), /at least one frame/);
    throws(() => quantizeFrames(pixels, 16, 16), /frames must be an array/);
    throws(() => quantizeFrames([pixels, pixels.subarray(4)], 16, 16), /frame 1 has 1020 bytes/);
  });

  test("leave the module usable", () => {
    assert.throws(() => quantize(pixels, 1, 1));
    assert.equal(quantize(pixels, 16, 16).indices.length, 256);
  });
});

describe("package", () => {
  const manifest = JSON.parse(readFileSync(join(pkgDir, "package.json"), "utf8"));

  test("package.json lists existing files", () => {
    assert.equal(manifest.name, "libimgquant");
    assert.equal(manifest.license, "BSD-2-Clause");
    assert.equal(manifest.type, "module");
    for (const file of manifest.files) assert.ok(existsSync(join(pkgDir, file)), file);
    for (const target of Object.values(manifest.exports["."])) assert.ok(existsSync(join(pkgDir, target)), target);
  });

  test("resolves as `libimgquant` through the exports map", () => {
    const project = join(temp, "project");
    mkdirSync(join(project, "node_modules"), { recursive: true });
    symlinkSync(pkgDir, join(project, "node_modules", "libimgquant"), "dir");
    const script = join(project, "main.mjs");
    writeFileSync(
      script,
      'import { quantize } from "libimgquant";\n' +
        "const r = quantize(new Uint8Array(4 * 4 * 4).fill(128), 4, 4);\n" +
        "console.log(JSON.stringify([r.palette.length, r.indices.length]));\n",
    );
    const output = execFileSync(process.execPath, [script], { cwd: project, encoding: "utf8" });
    assert.deepEqual(JSON.parse(output), [4, 16]);
  });
});
