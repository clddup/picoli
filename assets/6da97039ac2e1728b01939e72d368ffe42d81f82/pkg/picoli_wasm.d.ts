/* tslint:disable */
/* eslint-disable */

export function baseline_capabilities(): string;

export function baseline_encode(input: Uint8Array, format: string, quality: number): Uint8Array;

/**
 * Optional compression strength percentage; input format and dimensions stay unchanged.
 * When `memory_mib` is omitted, the working-set budget adapts automatically to the image.
 * Proxy error limits are experimental and do not prove perceptual equivalence.
 */
export function compress_experimental(input: Uint8Array, strength_percent?: number | null, memory_mib?: number | null): Uint8Array;

/**
 * Header-only adaptive one-worker working-set estimate; not free RAM/RSS.
 * The returned JSON has width, height, format, required_memory_bytes and memory_mib.
 * Actual compression still validates the complete image.
 */
export function compression_memory_plan(input: Uint8Array): string;

export type InitInput = RequestInfo | URL | Response | BufferSource | WebAssembly.Module;

export interface InitOutput {
    readonly memory: WebAssembly.Memory;
    readonly baseline_capabilities: (a: number) => void;
    readonly baseline_encode: (a: number, b: number, c: number, d: number, e: number, f: number) => void;
    readonly compress_experimental: (a: number, b: number, c: number, d: number, e: number) => void;
    readonly compression_memory_plan: (a: number, b: number, c: number) => void;
    readonly __wbindgen_add_to_stack_pointer: (a: number) => number;
    readonly __wbindgen_export: (a: number, b: number, c: number) => void;
    readonly __wbindgen_export2: (a: number, b: number) => number;
    readonly __wbindgen_export3: (a: number, b: number, c: number, d: number) => number;
}

export type SyncInitInput = BufferSource | WebAssembly.Module;

/**
 * Instantiates the given `module`, which can either be bytes or
 * a precompiled `WebAssembly.Module`.
 *
 * @param {{ module: SyncInitInput }} module - Passing `SyncInitInput` directly is deprecated.
 *
 * @returns {InitOutput}
 */
export function initSync(module: { module: SyncInitInput } | SyncInitInput): InitOutput;

/**
 * If `module_or_path` is {RequestInfo} or {URL}, makes a request and
 * for everything else, calls `WebAssembly.instantiate` directly.
 *
 * @param {{ module_or_path: InitInput | Promise<InitInput> }} module_or_path - Passing `InitInput` directly is deprecated.
 *
 * @returns {Promise<InitOutput>}
 */
export default function __wbg_init (module_or_path?: { module_or_path: InitInput | Promise<InitInput> } | InitInput | Promise<InitInput>): Promise<InitOutput>;
