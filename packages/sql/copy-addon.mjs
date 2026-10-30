// Copies the built addon to sql.node: librestate_sql.dylib on macOS,
// librestate_sql.so elsewhere. Usage: node copy-addon.mjs release|debug
import { copyFileSync } from 'node:fs';

const profile = process.argv[2] ?? 'release';
const ext = process.platform === 'darwin' ? 'dylib' : 'so';
const dir = process.env.CARGO_TARGET_DIR || 'target';
copyFileSync(`${dir}/${profile}/librestate_sql.${ext}`, 'sql.node');
