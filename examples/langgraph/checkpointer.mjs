// A LangGraph checkpointer on a sqlite-kv database: `SqlCheckpointSaver`, over
// a database session `db` (in Restate, the `db` of `ctx.sqlSession`), which
// has `query`, `execute` and `transaction(body)`.
//
// Three tables, with values as LangGraph's serializer writes them:
// - `checkpoints`: each checkpoint, without its channel values.
// - `channel_values`: each channel's value at each version, stored when a
//   step changes it. A checkpoint's values are those at its channel versions,
//   so a step stores only what it changed, not the whole state again (an
//   agent's message list, say).
// - `writes`: the pending writes of each checkpoint's tasks.
//
// Each `put` and `putWrites` is one SQL transaction, so a step is stored whole,
// in order with everything else the handler does. Operations run one at a
// time, in call order: LangGraph writes the results of parallel tasks at once.
import {
  BaseCheckpointSaver,
  copyCheckpoint,
  maxChannelVersion,
  TASKS,
  WRITES_IDX_MAP,
} from '@langchain/langgraph-checkpoint';

const SCHEMA = `
  CREATE TABLE IF NOT EXISTS checkpoints (
    thread_id TEXT NOT NULL,
    checkpoint_ns TEXT NOT NULL,
    checkpoint_id TEXT NOT NULL,
    parent_checkpoint_id TEXT,
    type TEXT NOT NULL,
    checkpoint BLOB NOT NULL,
    metadata BLOB NOT NULL,
    PRIMARY KEY (thread_id, checkpoint_ns, checkpoint_id)
  );
  CREATE TABLE IF NOT EXISTS channel_values (
    thread_id TEXT NOT NULL,
    checkpoint_ns TEXT NOT NULL,
    channel TEXT NOT NULL,
    version TEXT NOT NULL,
    type TEXT NOT NULL,
    value BLOB,
    PRIMARY KEY (thread_id, checkpoint_ns, channel, version)
  );
  CREATE TABLE IF NOT EXISTS writes (
    thread_id TEXT NOT NULL,
    checkpoint_ns TEXT NOT NULL,
    checkpoint_id TEXT NOT NULL,
    task_id TEXT NOT NULL,
    idx INTEGER NOT NULL,
    channel TEXT NOT NULL,
    type TEXT NOT NULL,
    value BLOB NOT NULL,
    PRIMARY KEY (thread_id, checkpoint_ns, checkpoint_id, task_id, idx)
  );`;

const COLUMNS = 'thread_id, checkpoint_ns, checkpoint_id, parent_checkpoint_id, type, checkpoint, metadata';

// The type of a channel a step emptied.
const EMPTY = 'empty';

/** Whether `actual` has every field of `expected`, recursively (a metadata filter). */
function matches(actual, expected) {
  if (expected === null || typeof expected !== 'object') return actual === expected;
  if (actual === null || typeof actual !== 'object') return false;
  if (Array.isArray(expected) !== Array.isArray(actual)) return false;
  if (Array.isArray(expected) && expected.length !== actual.length) return false;
  return Object.entries(expected).every(([key, value]) => matches(actual[key], value));
}

export class SqlCheckpointSaver extends BaseCheckpointSaver {
  #db;
  #ready;
  #tail = Promise.resolve();

  constructor(db, serde) {
    super(serde);
    this.#db = db;
  }

  /** Runs `operation` after the ones before it, on a database with the tables. */
  #serial(operation) {
    this.#ready ??= this.#db.execute(SCHEMA);
    const result = this.#tail.then(() => this.#ready).then(operation);
    this.#tail = result.catch(() => {});
    return result;
  }

  /** The values of `checkpoint`'s channels, at its versions. */
  async #channelValues(threadId, ns, checkpoint) {
    const versions = Object.entries(checkpoint.channel_versions ?? {});
    const values = {};
    if (versions.length === 0) return values;
    const rows = await this.#db.query(
      `SELECT channel, type, value FROM channel_values WHERE thread_id = ? AND checkpoint_ns = ? AND (${versions
        .map(() => '(channel = ? AND version = ?)')
        .join(' OR ')})`,
      [threadId, ns, ...versions.flatMap(([channel, version]) => [channel, String(version)])],
    );
    for (const [channel, type, value] of rows) {
      if (type !== EMPTY) values[channel] = await this.serde.loadsTyped(type, value);
    }
    return values;
  }

  async #tuple([threadId, ns, checkpointId, parentId, type, stored, metadata]) {
    const checkpoint = await this.serde.loadsTyped(type, stored);
    checkpoint.channel_values = await this.#channelValues(threadId, ns, checkpoint);
    // A checkpoint of an earlier format (v < 4) keeps its pending sends as
    // writes of its parent.
    if (checkpoint.v < 4 && parentId !== null) await this.#migratePendingSends(checkpoint, threadId, ns, parentId);
    const writes = await this.#db.query(
      'SELECT task_id, channel, type, value FROM writes WHERE thread_id = ? AND checkpoint_ns = ? AND checkpoint_id = ? ORDER BY rowid',
      [threadId, ns, checkpointId],
    );
    const config = (id) => ({ configurable: { thread_id: threadId, checkpoint_ns: ns, checkpoint_id: id } });
    return {
      config: config(checkpointId),
      checkpoint,
      metadata: await this.serde.loadsTyped(type, metadata),
      parentConfig: parentId === null ? undefined : config(parentId),
      pendingWrites: await Promise.all(
        writes.map(async ([taskId, channel, writeType, value]) => [
          taskId,
          channel,
          await this.serde.loadsTyped(writeType, value),
        ]),
      ),
    };
  }

  async #migratePendingSends(checkpoint, threadId, ns, parentId) {
    const sends = await this.#db.query(
      'SELECT type, value FROM writes WHERE thread_id = ? AND checkpoint_ns = ? AND checkpoint_id = ? AND channel = ? ORDER BY rowid',
      [threadId, ns, parentId, TASKS],
    );
    checkpoint.channel_values[TASKS] = await Promise.all(sends.map(([type, value]) => this.serde.loadsTyped(type, value)));
    const versions = Object.values(checkpoint.channel_versions);
    checkpoint.channel_versions[TASKS] =
      versions.length > 0 ? maxChannelVersion(...versions) : this.getNextVersion(undefined);
  }

  getTuple(config) {
    return this.#serial(async () => {
      const { thread_id, checkpoint_ns = '', checkpoint_id } = config.configurable ?? {};
      const rows = checkpoint_id
        ? await this.#db.query(
            `SELECT ${COLUMNS} FROM checkpoints WHERE thread_id = ? AND checkpoint_ns = ? AND checkpoint_id = ?`,
            [thread_id, checkpoint_ns, checkpoint_id],
          )
        : await this.#db.query(
            `SELECT ${COLUMNS} FROM checkpoints WHERE thread_id = ? AND checkpoint_ns = ? ORDER BY checkpoint_id DESC LIMIT 1`,
            [thread_id, checkpoint_ns],
          );
      return rows.length === 0 ? undefined : this.#tuple(rows[0]);
    });
  }

  async *list(config, { limit, before, filter } = {}) {
    const tuples = await this.#serial(async () => {
      const { thread_id, checkpoint_ns } = config.configurable ?? {};
      const where = [];
      const params = [];
      if (thread_id !== undefined) where.push('thread_id = ?') && params.push(thread_id);
      if (checkpoint_ns !== undefined && checkpoint_ns !== null) where.push('checkpoint_ns = ?') && params.push(checkpoint_ns);
      const beforeId = before?.configurable?.checkpoint_id;
      if (beforeId !== undefined) where.push('checkpoint_id < ?') && params.push(beforeId);
      const rows = await this.#db.query(
        `SELECT ${COLUMNS} FROM checkpoints ${where.length ? `WHERE ${where.join(' AND ')}` : ''} ORDER BY checkpoint_id DESC`,
        params,
      );
      const wanted = Object.fromEntries(Object.entries(filter ?? {}).filter(([, value]) => value !== undefined));
      const found = [];
      for (const row of rows) {
        if (limit !== undefined && found.length >= limit) break;
        const tuple = await this.#tuple(row);
        if (matches(tuple.metadata, wanted)) found.push(tuple);
      }
      return found;
    });
    yield* tuples;
  }

  put(config, checkpoint, metadata, newVersions) {
    return this.#serial(async () => {
      const { thread_id, checkpoint_ns = '', checkpoint_id: parentId } = config.configurable ?? {};
      if (!thread_id) throw new Error('Missing "thread_id" field in passed "config.configurable".');
      const { channel_values: values, ...rest } = copyCheckpoint(checkpoint);
      const [type, stored] = await this.serde.dumpsTyped(rest);
      const [metadataType, metadataValue] = await this.serde.dumpsTyped(metadata);
      if (type !== metadataType) throw new Error('Failed to serialize checkpoint and metadata to the same type.');
      // The channels this step changed, at their new versions.
      const changed = await Promise.all(
        Object.entries(newVersions ?? {}).map(async ([channel, version]) => [
          channel,
          String(version),
          ...(channel in values ? await this.serde.dumpsTyped(values[channel]) : [EMPTY, null]),
        ]),
      );
      await this.#db.transaction(async (tx) => {
        for (const row of changed) {
          await tx.execute(
            'INSERT OR REPLACE INTO channel_values (thread_id, checkpoint_ns, channel, version, type, value) VALUES (?, ?, ?, ?, ?, ?)',
            [thread_id, checkpoint_ns, ...row],
          );
        }
        await tx.execute(`INSERT OR REPLACE INTO checkpoints (${COLUMNS}) VALUES (?, ?, ?, ?, ?, ?, ?)`, [
          thread_id,
          checkpoint_ns,
          checkpoint.id,
          parentId ?? null,
          type,
          stored,
          metadataValue,
        ]);
      });
      return { configurable: { thread_id, checkpoint_ns, checkpoint_id: checkpoint.id } };
    });
  }

  putWrites(config, writes, taskId) {
    return this.#serial(async () => {
      const { thread_id, checkpoint_ns = '', checkpoint_id } = config.configurable ?? {};
      if (!thread_id) throw new Error('Missing thread_id field in config.configurable.');
      if (!checkpoint_id) throw new Error('Missing checkpoint_id field in config.configurable.');
      // Special writes (errors, interrupts) replace; others keep the first.
      const verb = writes.every(([channel]) => channel in WRITES_IDX_MAP) ? 'INSERT OR REPLACE' : 'INSERT OR IGNORE';
      const rows = await Promise.all(
        writes.map(async ([channel, value], idx) => [
          thread_id,
          checkpoint_ns,
          checkpoint_id,
          taskId,
          WRITES_IDX_MAP[channel] ?? idx,
          channel,
          ...(await this.serde.dumpsTyped(value)),
        ]),
      );
      await this.#db.transaction(async (tx) => {
        for (const row of rows) {
          await tx.execute(
            `${verb} INTO writes (thread_id, checkpoint_ns, checkpoint_id, task_id, idx, channel, type, value) VALUES (?, ?, ?, ?, ?, ?, ?, ?)`,
            row,
          );
        }
      });
    });
  }

  deleteThread(threadId) {
    return this.#serial(() =>
      this.#db.transaction(async (tx) => {
        for (const table of ['checkpoints', 'channel_values', 'writes']) {
          await tx.execute(`DELETE FROM ${table} WHERE thread_id = ?`, [threadId]);
        }
      }),
    );
  }
}
