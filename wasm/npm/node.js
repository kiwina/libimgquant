// Node.js: read the .wasm next to this file and compile it synchronously,
// since Node's fetch() can't read file: URLs.
import { readFileSync } from "node:fs";
import { initSync } from "./libimgquant.js";

initSync({ module: readFileSync(new URL("./libimgquant_bg.wasm", import.meta.url)) });

export { compressPng, quantize, quantizeFrames } from "./libimgquant.js";
