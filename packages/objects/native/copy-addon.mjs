// Copies the built addon to objects.node, beside this file:
// librestate_objects.dylib on macOS, librestate_objects.so elsewhere.
// Usage: node copy-addon.mjs release|debug
import { copyFileSync } from 'node:fs';

const profile = process.argv[2] ?? 'release';
const ext = process.platform === 'darwin' ? 'dylib' : 'so';
const here = new URL('.', import.meta.url);
const target = process.env.CARGO_TARGET_DIR ? `${process.env.CARGO_TARGET_DIR}/` : new URL('target/', here);
copyFileSync(new URL(`${profile}/librestate_objects.${ext}`, target), new URL('objects.node', here));
