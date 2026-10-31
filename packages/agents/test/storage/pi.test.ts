// pi-durable's storage conformance suite on restate-agents/pi/storage, over an SQL
// database in memory: `pnpm --filter restate-agents test pi`.
import { registerStorageConformance } from '@earendil-works/pi-durable/testing';
import { asDatabase, open } from 'restate-sessions/internal';
import { describe, expect, it } from 'vitest';
import { memoryStore } from 'restate-sessions/test/support';
import { openSqlKvStorage } from 'restate-agents/pi/storage';

let n = 0;
registerStorageConformance({ describe, expect, it }, 'restate-sessions', async (use) => {
  const db = await open(`conformance-${process.pid}-${n++}`, memoryStore());
  try {
    await use(await openSqlKvStorage(asDatabase(db)));
  } finally {
    await db.close();
  }
});
