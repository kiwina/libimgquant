// The module is ready on import: the WebAssembly is loaded by index.js or node.js.
export { compressPng, quantize, quantizeFrames } from "./libimgquant.js";
export type { FramesResult, Options, Pixels, QuantizeResult } from "./libimgquant.js";
