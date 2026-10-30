// Fault injection for crash tests: with ABORT_PER_MILLE, the process aborts
// before or after random requests of the objects' stores; and with ABORT_AT,
// at that request (counted from 1), so that it aborts at least once.
import * as restate from '@restatedev/restate-sdk';
import { setStoreWrapper } from 'restate-objects/internal';
import { fileURLToPath } from 'node:url';

const perMille = Number(process.env.ABORT_PER_MILLE ?? 0);
const at = Number(process.env.ABORT_AT ?? 0);
let requests = 0;

const maybeAbort = () => {
  if (++requests === at || Math.random() * 1000 < perMille) {
    console.error(`aborting at state request ${requests}`);
    // As abruptly as process.abort(), without a core dump or a crash report.
    process.kill(process.pid, 'SIGKILL');
  }
};

if (perMille > 0) {
  setStoreWrapper((store) => ({
    get: async (key) => {
      maybeAbort();
      return store.get(key);
    },
    set: (key, value) => {
      maybeAbort();
      store.set(key, value);
      maybeAbort();
    },
    clear: (key) => {
      maybeAbort();
      store.clear(key);
      maybeAbort();
    },
    sync: async () => {
      maybeAbort();
      return store.sync();
    },
    // A store that cannot list its keys stays one: it is never swept.
    keys:
      store.keys &&
      (async () => {
        maybeAbort();
        return store.keys!();
      }),
  }));
}

/** Serve `services` when `meta` is the entry module's `import.meta`. */
export function serveIfMain(meta: ImportMeta, services: restate.ServeOptions['services']) {
  if (process.argv[1] && fileURLToPath(meta.url) === process.argv[1]) {
    void restate.serve({ services, port: Number(process.env.PORT ?? 9080) });
  }
}
