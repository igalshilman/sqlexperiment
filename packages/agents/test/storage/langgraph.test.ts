// LangGraph's checkpointer conformance suite on checkpointer.ts, over a
// SQL database in memory: `pnpm --filter restate-agents test langgraph`.
import { validate } from '@langchain/langgraph-checkpoint-validation';
import { asDatabase, open, type Connection } from 'restate-sessions/internal';
import { memoryStore } from 'restate-sessions/test/support';
import { SqlCheckpointSaver } from 'restate-agents/langgraph';

let n = 0;
const databases = new WeakMap<SqlCheckpointSaver, Connection>();
validate({
  checkpointerName: 'restate-sessions',
  async createCheckpointer() {
    const db = await open(`langgraph-${process.pid}-${n++}`, memoryStore());
    const saver = new SqlCheckpointSaver(asDatabase(db));
    databases.set(saver, db);
    return saver;
  },
  async destroyCheckpointer(saver) {
    await databases.get(saver)?.close();
  },
});
