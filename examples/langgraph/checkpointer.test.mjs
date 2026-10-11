// LangGraph's checkpointer conformance suite on checkpointer.mjs, over a
// sqlite-kv database in memory: `npm run test:langgraph` (or make test-langgraph).
import { validate } from '@langchain/langgraph-checkpoint-validation';
import { open } from 'sqlite-kv';
import { SqlCheckpointSaver } from './checkpointer.mjs';

/** A sqlite-kv session over a fresh in-memory store, as `ctx.sqlSession` gives one. */
async function memoryDatabase(name) {
  const map = new Map();
  const store = {
    get: async (key) => map.get(key),
    set: (key, value) => map.set(key, Uint8Array.from(value)),
    clear: (key) => map.delete(key),
    sync: async () => {},
  };
  return open(name, store);
}

let n = 0;
const databases = new WeakMap();
validate({
  checkpointerName: 'sqlite-kv',
  async createCheckpointer() {
    const db = await memoryDatabase(`langgraph-${process.pid}-${n++}`);
    const saver = new SqlCheckpointSaver(db);
    databases.set(saver, db);
    return saver;
  },
  async destroyCheckpointer(saver) {
    await databases.get(saver).close();
  },
});
