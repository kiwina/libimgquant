// Browsers, workers and bundlers: fetch and compile the .wasm next to this file once, on
// import (top-level await). Node.js loads node.js instead (the "node" export condition).
import init from "./libimgquant.js";

await init({ module_or_path: new URL("./libimgquant_bg.wasm", import.meta.url) });

export { compressPng, quantize, quantizeFrames } from "./libimgquant.js";
