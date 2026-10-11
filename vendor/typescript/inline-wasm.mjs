// Inline a wasm-bindgen `--target web` build into one JavaScript module that
// instantiates synchronously, as the Restate TypeScript SDK ships its core.
// (wasm-pack-inline 0.1.4 slices by text positions that newer wasm-bindgen
// output no longer has.)
//
// usage: node inline-wasm.mjs <pkg dir> <name> <out dir>
import { readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';

const [pkg, name, out] = process.argv.slice(2);
const base64 = readFileSync(join(pkg, `${name}_bg.wasm`)).toString('base64');
let js = readFileSync(join(pkg, `${name}.js`), 'utf8');

function cut(text, from, to) {
  const start = text.indexOf(from);
  const end = to === undefined ? text.length : text.indexOf(to, start);
  if (start < 0 || end < 0) throw new Error(`marker not found: ${from}`);
  return text.slice(0, start) + text.slice(end);
}

// Asynchronous loading needs a URL or fetch; this module has the bytes.
js = cut(js, 'async function __wbg_load(', 'function initSync(');
js = cut(js, 'function initSync(');

const dts = readFileSync(join(pkg, `${name}.d.ts`), 'utf8');
const exportsEnd = dts.indexOf('export type InitInput');

writeFileSync(
  join(out, `${name}.js`),
  `const __wasm_base64__ = "${base64}";

function __decode_base64__(base64) {
  const binary = atob(base64);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
  return bytes;
}

${js}
function initSync() {
  if (wasm !== undefined) return wasm;
  const module = new WebAssembly.Module(__decode_base64__(__wasm_base64__));
  const instance = new WebAssembly.Instance(module, __wbg_get_imports());
  return __wbg_finalize_init(instance, module);
}

initSync();
`,
);
writeFileSync(join(out, `${name}.d.ts`), exportsEnd < 0 ? dts : dts.slice(0, exportsEnd));
