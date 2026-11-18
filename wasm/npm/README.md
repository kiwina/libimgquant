# libimgquant

Converts RGBA images to 8-bit palette images with alpha, for small PNGs and GIFs, in
browsers and Node.js. `compressPng` does what pngquant does: a PNG file in, a palette PNG
out, about 30% of the input size in total on the project's private 22-image test set.

This is [libimgquant 3](https://github.com/kiwina/libimgquant), a pure-Rust color quantizer
based on libimagequant 2.4.1 (the last BSD-licensed release of the library behind pngquant)
and improved with color quantization research, compiled to WebAssembly.

- **BSD-2-Clause**, no GPL code.
- **One ES module**, no dependencies, TypeScript types included.
- **Size:** the WebAssembly module is about 480 KB (190 KB gzipped), PNG decoder and encoder included.
- **Runs in** current browsers, module Web Workers and bundlers, and Node.js 18 or later.
  It is single-threaded; run it in a Worker to keep a page responsive.

## Install

```bash
npm install libimgquant
```

The module loads its WebAssembly when it is imported (top-level `await` in browsers), so the
functions can be called right away.

## Node.js

```js
import { readFile, writeFile } from "node:fs/promises";
import { compressPng } from "libimgquant";

const input = await readFile("photo.png");
const output = compressPng(input, { quality: [65, 90] });
await writeFile("photo-fs8.png", output);
```

## Browser

```js
import { compressPng, quantize } from "libimgquant";

// A PNG file, for example from <input type="file"> or fetch()
const file = document.querySelector("input[type=file]").files[0];
const png = compressPng(new Uint8Array(await file.arrayBuffer()));
const url = URL.createObjectURL(new Blob([png], { type: "image/png" }));

// Or pixels from a canvas
const { data, width, height } = canvas.getContext("2d").getImageData(0, 0, canvas.width, canvas.height);
const { palette, indices, quality } = quantize(data, width, height, { maxColors: 64 });
```

### Other image formats

`compressPng` reads PNG only. For any other format the browser can decode (JPEG, WebP, AVIF,
GIF, ...), draw the image on a canvas, save the canvas as PNG and compress that:

```js
const bitmap = await createImageBitmap(file);
const canvas = document.createElement("canvas");
canvas.width = bitmap.width;
canvas.height = bitmap.height;
canvas.getContext("2d").drawImage(bitmap, 0, 0);
const blob = await new Promise((resolve) => canvas.toBlob(resolve, "image/png"));
const png = compressPng(new Uint8Array(await blob.arrayBuffer()));
```

In a Worker, use `new OffscreenCanvas(bitmap.width, bitmap.height)` and
`await canvas.convertToBlob({ type: "image/png" })` instead. To skip the PNG step, pass
`getImageData(...).data` to `quantize` and encode the palette and indices yourself.

### Bundlers

Vite and webpack 5 emit the `.wasm` file automatically: they follow
`new URL("./libimgquant_bg.wasm", import.meta.url)`. With Vite, add
`optimizeDeps: { exclude: ["libimgquant"] }` so that the dev server serves the file from the
package; with Vite 6 or older, also set `build.target: "es2022"`, because the module uses
top-level `await`. esbuild and Rollup don't copy the file: copy
`node_modules/libimgquant/libimgquant_bg.wasm` next to the output bundle. Without a bundler,
serve the package directory and import `index.js` from it; the server should send `.wasm`
files as `application/wasm`.

## API

### `quantize(rgba, width, height, options?)`

Quantizes an image and remaps it to the palette.

- `rgba`: `Uint8Array`, `Uint8ClampedArray` (such as `ImageData.data`), `Buffer` or
  `ArrayBuffer` with 4 bytes per pixel, row by row.
- Returns `{ palette, indices, quality }`:
  - `palette`: `Uint8Array` of RGBA bytes, 4 per color, at most 256 colors. Translucent
    colors come first, so a PNG tRNS chunk can stop early.
  - `indices`: `Uint8Array` with one palette index per pixel.
  - `quality`: the quality reached, 0-100, or `undefined`.

### `compressPng(png, options?)`

Converts a PNG file (any color type and bit depth) to a palette PNG and returns its bytes
as a `Uint8Array`. The output keeps the input's gamma and ICC color profile, uses the
smallest bit depth that holds the palette, and is the smaller of an unfiltered and a
filtered encoding.

### `quantizeFrames(frames, width, height, options?)`

Quantizes several images of the same size (animation frames, sprites, tiles) to one shared
palette, for example the global color table of an animated GIF. Every frame's colors count,
so colors that appear in only one frame still get palette entries.

- `frames`: an array of RGBA byte arrays, as for `quantize`.
- Returns `{ palette, frames }`: the shared palette (RGBA bytes) and each frame's indices
  (`Uint8Array[]`, in the input order).

### Options

| Option | Values | Default | |
| --- | --- | --- | --- |
| `maxColors` | 2-256 | 256 | Maximum number of palette colors. |
| `speed` | 1-10 | 3 | 1 is slowest and best, 10 fastest. |
| `quality` | `{ min, max }` or `[min, max]`, 0-100 | no limit | Like JPEG quality: use the fewest colors that reach `max`; throw if `min` can't be reached. `{ min }` alone means up to 100, `{ max }` alone from 0. |
| `dithering` | 0-1, or `true`/`false` | 1 | Floyd-Steinberg dithering level. |
| `profile` | `"improved"`, `"compat"` | `"improved"` | `"compat"` reproduces libimagequant 2.4.1's algorithms. |

Unknown option names are errors, so a misspelt option is not silently ignored.

### Errors

Every function throws an `Error` with a message that says what is wrong, for example:

- `quality too low: the image can't reach the minimum quality 90 with 2 colors`
- `the pixel data has 1000 bytes, but 16x16 RGBA pixels need 1024 (4 bytes per pixel)`
- `maxColors must be an integer from 2 to 256, not 300`
- `unknown option 'colors'; the options are maxColors, speed, quality, dithering and profile`
- `invalid PNG: ...`

```js
try {
  output = compressPng(input, { quality: [65, 90] });
} catch (e) {
  // e.message starts with "quality too low" when the minimum quality can't be reached;
  // keep the original file then, as pngquant does
}
```

## License

BSD-2-Clause, plus the original permissive notice of libimagequant 2.4.1; see `LICENSE` and
`NOTICE`.
The WebAssembly module also contains the Rust standard library and other permissively
licensed Rust crates (the PNG codec and the JS bindings); their licenses are in
`THIRD-PARTY-LICENSES.txt`.
