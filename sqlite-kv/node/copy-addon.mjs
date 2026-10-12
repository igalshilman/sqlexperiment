// Copies the built addon to sqlite_kv.node: libsqlite_kv_node.dylib on macOS,
// libsqlite_kv_node.so elsewhere. Usage: node copy-addon.mjs release|debug
import { copyFileSync } from 'node:fs';

const profile = process.argv[2] ?? 'release';
const ext = process.platform === 'darwin' ? 'dylib' : 'so';
const dir = process.env.CARGO_TARGET_DIR || 'target';
copyFileSync(`${dir}/${profile}/libsqlite_kv_node.${ext}`, 'sqlite_kv.node');
