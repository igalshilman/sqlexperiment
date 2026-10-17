// LangGraph's checkpointer conformance suite on checkpointer.ts, over a
// sqlite-kv database in memory: `make test-ts T=langgraph`.
import { validate } from '@langchain/langgraph-checkpoint-validation';
import { open, type Database } from 'sqlite-kv';
import { memoryStore } from '../testing.ts';
import { SqlCheckpointSaver } from 'restate-agents/langgraph';

let n = 0;
const databases = new WeakMap<SqlCheckpointSaver, Database>();
validate({
  checkpointerName: 'sqlite-kv',
  async createCheckpointer() {
    const db = await open(`langgraph-${process.pid}-${n++}`, memoryStore());
    const saver = new SqlCheckpointSaver(db);
    databases.set(saver, db);
    return saver;
  },
  async destroyCheckpointer(saver) {
    await databases.get(saver)?.close();
  },
});
