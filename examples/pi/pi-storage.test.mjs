// pi-durable's storage conformance suite on pi-storage.mjs, over a sqlite-kv
// database in memory: `npm run test:pi` (or `make test-pi`).
import { registerStorageConformance } from '@earendil-works/pi-durable/testing';
import { open } from 'sqlite-kv';
import { describe, expect, it } from 'vitest';
import { openSqlKvStorage } from './pi-storage.mjs';

let n = 0;
registerStorageConformance({ describe, expect, it }, 'sqlite-kv', async (use) => {
  const map = new Map();
  const store = {
    get: async (key) => map.get(key),
    set: (key, value) => map.set(key, Uint8Array.from(value)),
    clear: (key) => map.delete(key),
    sync: async () => {},
  };
  const db = await open(`conformance-${process.pid}-${n++}`, store);
  try {
    await use(await openSqlKvStorage(db));
  } finally {
    await db.close();
  }
});
