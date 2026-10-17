// pi-durable's storage conformance suite on restate-agents/pi/storage, over a sqlite-kv
// database in memory: `make test-ts T=pi`.
import { registerStorageConformance } from '@earendil-works/pi-durable/testing';
import { open } from 'sqlite-kv';
import { describe, expect, it } from 'vitest';
import { memoryStore } from '../testing.ts';
import { openSqlKvStorage } from 'restate-agents/pi/storage';

let n = 0;
registerStorageConformance({ describe, expect, it }, 'sqlite-kv', async (use) => {
  const db = await open(`conformance-${process.pid}-${n++}`, memoryStore());
  try {
    await use(await openSqlKvStorage(db));
  } finally {
    await db.close();
  }
});
