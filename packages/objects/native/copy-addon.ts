// Copies the built addon into the package of this machine's platform
// (../npm/<platform>/objects.node, see ../src/platform.ts), where
// restate-session-objects loads it: librestate_session_objects.dylib on macOS,
// librestate_session_objects.so elsewhere.
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
const built = new URL(`${profile}/librestate_session_objects.${ext}`, target);
copyFileSync(built, new URL(`../npm/${platform()}/objects.node`, here));
