// Copies the built addon into the package of this machine's platform
// (../npm/<platform>/sessions.node, see ../src/platform.ts), where
// restate-sessions loads it: librestate_sessions.dylib on macOS,
// librestate_sessions.so elsewhere.
// Usage: node copy-addon.ts release|debug|dist
import { copyFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
import { platform } from '../src/platform.ts';

const [profile = 'release'] = process.argv.slice(2);
const ext = process.platform === 'darwin' ? 'dylib' : 'so';
const here = new URL('.', import.meta.url);
const target = process.env.CARGO_TARGET_DIR
  ? pathToFileURL(`${resolve(process.env.CARGO_TARGET_DIR)}/`)
  : new URL('target/', here);
const built = new URL(`${profile}/librestate_sessions.${ext}`, target);
copyFileSync(built, new URL(`../npm/${platform()}/sessions.node`, here));
