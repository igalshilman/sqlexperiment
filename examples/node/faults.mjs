// Fault injection for crash tests: with ABORT_PER_MILLE, the process aborts
// before or after random requests of the objects' stores.
import * as restate from '@restatedev/restate-sdk';
import { fileURLToPath } from 'node:url';

const perMille = Number(process.env.ABORT_PER_MILLE ?? 0);

const maybeAbort = () => {
  if (Math.random() * 1000 < perMille) {
    console.error('aborting at a state request');
    process.abort();
  }
};

if (perMille > 0) {
  restate.internal.setSqlStoreWrapper((store) => ({
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
  }));
}

/** Serve `services` when `meta` is the entry module's `import.meta`. */
export function serveIfMain(meta, services) {
  if (process.argv[1] && fileURLToPath(meta.url) === process.argv[1]) {
    restate.serve({ services, port: Number(process.env.PORT ?? 9080) });
  }
}
