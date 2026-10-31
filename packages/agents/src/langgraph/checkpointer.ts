// A LangGraph checkpointer on an SQL database: `SqlCheckpointSaver`, over a
// database session `db` (in Restate, `ctx.db`), which has `query`, `execute`
// and `transaction(body)`.
//
// Three tables, with values as LangGraph's serializer writes them:
// - `restate_langgraph_checkpoints`: each checkpoint, without its channel values.
// - `restate_langgraph_channel_values`: each channel's value at each version, stored when a
//   step changes it. A checkpoint's values are those at its channel versions,
//   so a step stores only what it changed, not the whole state again (an
//   agent's message list, say). Versions are unique, as in LangGraph's own
//   savers (a count and a random part): a thread forked from an earlier
//   checkpoint takes the same counts again, and its values must not replace
//   those of the branch it forked from.
// - `restate_langgraph_writes`: the pending writes of each checkpoint's tasks.
//
// Each `put` and `putWrites` is one SQL transaction, so a step is stored whole,
// in order with everything else the handler does. Operations run one at a
// time, in call order: LangGraph writes the results of parallel tasks at once.
//
// By default a thread keeps every checkpoint (LangGraph's history: `list`,
// time travel, forks). With `history: 'latest'`, each `put` drops the
// thread's earlier checkpoints, their writes and the values only they had:
// a thread then takes the space of its state once, not once per step (a
// message list stored again at each step grows with the square of its
// length).
import { randomUUID } from 'node:crypto';
import type { RunnableConfig } from '@langchain/core/runnables';
import {
  BaseCheckpointSaver,
  copyCheckpoint,
  maxChannelVersion,
  TASKS,
  WRITES_IDX_MAP,
  type ChannelVersions,
  type Checkpoint,
  type CheckpointListOptions,
  type CheckpointMetadata,
  type CheckpointPendingWrite,
  type CheckpointTuple,
  type PendingWrite,
  type SerializerProtocol,
} from '@langchain/langgraph-checkpoint';
import type { SqlDatabase, SqlSession, SqlValue } from 'restate-objects';
import { Schema } from 'restate-objects/internal';

const SCHEMA = new Schema('restate-agents.langgraph', [
  `CREATE TABLE restate_langgraph_checkpoints (
    thread_id TEXT NOT NULL,
    checkpoint_ns TEXT NOT NULL,
    checkpoint_id TEXT NOT NULL,
    parent_checkpoint_id TEXT,
    type TEXT NOT NULL,
    checkpoint BLOB NOT NULL,
    metadata BLOB NOT NULL,
    PRIMARY KEY (thread_id, checkpoint_ns, checkpoint_id)
  )`,
  `CREATE TABLE restate_langgraph_channel_values (
    thread_id TEXT NOT NULL,
    checkpoint_ns TEXT NOT NULL,
    channel TEXT NOT NULL,
    version TEXT NOT NULL,
    type TEXT NOT NULL,
    value BLOB,
    PRIMARY KEY (thread_id, checkpoint_ns, channel, version)
  )`,
  `CREATE TABLE restate_langgraph_writes (
    thread_id TEXT NOT NULL,
    checkpoint_ns TEXT NOT NULL,
    checkpoint_id TEXT NOT NULL,
    task_id TEXT NOT NULL,
    idx INTEGER NOT NULL,
    channel TEXT NOT NULL,
    type TEXT NOT NULL,
    value BLOB NOT NULL,
    PRIMARY KEY (thread_id, checkpoint_ns, checkpoint_id, task_id, idx)
  )`,
]);

const COLUMNS = 'thread_id, checkpoint_ns, checkpoint_id, parent_checkpoint_id, type, checkpoint, metadata';

/** A row of `checkpoints`, in the order of COLUMNS. */
type CheckpointRow = [
  threadId: string,
  ns: string,
  checkpointId: string,
  parentId: string | null,
  type: string,
  checkpoint: Uint8Array,
  metadata: Uint8Array,
];

// The type of a channel a step emptied.
const EMPTY = 'empty';

/** Whether `actual` has every field of `expected`, recursively (a metadata filter). */
function matches(actual: unknown, expected: unknown): boolean {
  if (expected === null || typeof expected !== 'object') return actual === expected;
  if (actual === null || typeof actual !== 'object') return false;
  if (Array.isArray(expected) !== Array.isArray(actual)) return false;
  if (Array.isArray(expected) && expected.length !== (actual as unknown[]).length) return false;
  return Object.entries(expected).every(([key, value]) => matches((actual as Record<string, unknown>)[key], value));
}

/**
 * Drop what the thread `threadId` (namespace `ns`) keeps from before
 * `checkpoint`: its other checkpoints, their pending writes, and the channel
 * values at versions other than `checkpoint`'s.
 */
async function dropEarlier(tx: SqlDatabase, threadId: string, ns: string, checkpoint: Checkpoint) {
  const thread = [threadId, ns];
  for (const table of ['restate_langgraph_checkpoints', 'restate_langgraph_writes']) {
    await tx.execute(`DELETE FROM ${table} WHERE thread_id = ? AND checkpoint_ns = ? AND checkpoint_id <> ?`, [
      ...thread,
      checkpoint.id,
    ]);
  }
  const versions = Object.entries(checkpoint.channel_versions ?? {});
  const kept = versions.map(() => '(channel = ? AND version = ?)').join(' OR ') || '0';
  await tx.execute(
    `DELETE FROM restate_langgraph_channel_values WHERE thread_id = ? AND checkpoint_ns = ? AND NOT (${kept})`,
    [...thread, ...versions.flatMap(([channel, version]) => [channel, String(version)])],
  );
}

/** A LangGraph checkpointer whose threads are in the database session `db`: see the top of this file. */
export class SqlCheckpointSaver extends BaseCheckpointSaver {
  #db: SqlSession;
  #latest: boolean;
  #ready: Promise<void> | undefined;
  #tail: Promise<unknown> = Promise.resolve();

  /**
   * `db`: a database session (`ctx.db`). `history`: the checkpoints a thread
   * keeps, `all` (the default) or the `latest` only; `serde`: LangGraph's
   * serializer, by default its own.
   */
  constructor(
    db: SqlSession,
    { history = 'all', serde }: { history?: 'all' | 'latest'; serde?: SerializerProtocol } = {},
  ) {
    super(serde);
    this.#db = db;
    this.#latest = history === 'latest';
  }

  /**
   * The version after `current`: its count plus one, zero-padded so that
   * versions compare as their counts do, and a random part, so that branches
   * of a thread never share one. A string: LangGraph compares string versions
   * as strings, though `compile` types a saver's versions as numbers.
   */
  override getNextVersion(current: number | undefined): number {
    const count = current === undefined ? 0 : Number.parseInt(String(current).split('.')[0]!, 10);
    return `${String(count + 1).padStart(32, '0')}.${randomUUID()}` as unknown as number;
  }

  /** Runs `operation` after the ones before it, on a database with the tables. */
  #serial<T>(operation: () => Promise<T>): Promise<T> {
    this.#ready ??= SCHEMA.ensure(this.#db);
    const result = this.#tail.then(() => this.#ready).then(operation);
    this.#tail = result.catch(() => {});
    return result;
  }

  /** The values of `checkpoint`'s channels, at its versions. */
  async #channelValues(threadId: string, ns: string, checkpoint: Checkpoint): Promise<Record<string, unknown>> {
    const versions = Object.entries(checkpoint.channel_versions ?? {});
    const values: Record<string, unknown> = {};
    if (versions.length === 0) return values;
    const rows = await this.#db.values(
      `SELECT channel, type, value FROM restate_langgraph_channel_values WHERE thread_id = ? AND checkpoint_ns = ? AND (${versions
        .map(() => '(channel = ? AND version = ?)')
        .join(' OR ')})`,
      [threadId, ns, ...versions.flatMap(([channel, version]) => [channel, String(version)])],
    );
    for (const [channel, type, value] of rows as [string, string, Uint8Array][]) {
      if (type !== EMPTY) values[channel] = await this.serde.loadsTyped(type, value);
    }
    return values;
  }

  async #tuple([
    threadId,
    ns,
    checkpointId,
    parentId,
    type,
    stored,
    metadata,
  ]: CheckpointRow): Promise<CheckpointTuple> {
    const checkpoint: Checkpoint = await this.serde.loadsTyped(type, stored);
    checkpoint.channel_values = await this.#channelValues(threadId, ns, checkpoint);
    // A checkpoint of an earlier format (v < 4) keeps its pending sends as
    // writes of its parent.
    if (checkpoint.v < 4 && parentId !== null) await this.#migratePendingSends(checkpoint, threadId, ns, parentId);
    const writes = await this.#db.values(
      'SELECT task_id, channel, type, value FROM restate_langgraph_writes WHERE thread_id = ? AND checkpoint_ns = ? AND checkpoint_id = ? ORDER BY rowid',
      [threadId, ns, checkpointId],
    );
    const config = (id: string) => ({ configurable: { thread_id: threadId, checkpoint_ns: ns, checkpoint_id: id } });
    return {
      config: config(checkpointId),
      checkpoint,
      metadata: await this.serde.loadsTyped(type, metadata),
      parentConfig: parentId === null ? undefined : config(parentId),
      pendingWrites: await Promise.all(
        (writes as [string, string, string, Uint8Array][]).map(
          async ([taskId, channel, writeType, value]): Promise<CheckpointPendingWrite> => [
            taskId,
            channel,
            await this.serde.loadsTyped(writeType, value),
          ],
        ),
      ),
    };
  }

  async #migratePendingSends(checkpoint: Checkpoint, threadId: string, ns: string, parentId: string) {
    const sends = await this.#db.values(
      'SELECT type, value FROM restate_langgraph_writes WHERE thread_id = ? AND checkpoint_ns = ? AND checkpoint_id = ? AND channel = ? ORDER BY rowid',
      [threadId, ns, parentId, TASKS],
    );
    checkpoint.channel_values[TASKS] = await Promise.all(
      (sends as [string, Uint8Array][]).map(([type, value]) => this.serde.loadsTyped(type, value)),
    );
    const versions = Object.values(checkpoint.channel_versions);
    checkpoint.channel_versions[TASKS] =
      versions.length > 0 ? maxChannelVersion(...versions) : this.getNextVersion(undefined);
  }

  getTuple(config: RunnableConfig): Promise<CheckpointTuple | undefined> {
    return this.#serial(async () => {
      const { thread_id, checkpoint_ns = '', checkpoint_id } = config.configurable ?? {};
      const rows = checkpoint_id
        ? await this.#db.values(
            `SELECT ${COLUMNS} FROM restate_langgraph_checkpoints WHERE thread_id = ? AND checkpoint_ns = ? AND checkpoint_id = ?`,
            [thread_id, checkpoint_ns, checkpoint_id],
          )
        : await this.#db.values(
            `SELECT ${COLUMNS} FROM restate_langgraph_checkpoints WHERE thread_id = ? AND checkpoint_ns = ? ORDER BY checkpoint_id DESC LIMIT 1`,
            [thread_id, checkpoint_ns],
          );
      return rows.length === 0 ? undefined : this.#tuple(rows[0] as CheckpointRow);
    });
  }

  async *list(
    config: RunnableConfig,
    { limit, before, filter }: CheckpointListOptions = {},
  ): AsyncGenerator<CheckpointTuple> {
    const tuples = await this.#serial(async () => {
      const { thread_id, checkpoint_ns } = config.configurable ?? {};
      const where: string[] = [];
      const params: SqlValue[] = [];
      const filters: [string, SqlValue | undefined][] = [
        ['thread_id = ?', thread_id],
        ['checkpoint_ns = ?', checkpoint_ns ?? undefined],
        ['checkpoint_id < ?', before?.configurable?.checkpoint_id],
      ];
      for (const [condition, value] of filters) {
        if (value === undefined) continue;
        where.push(condition);
        params.push(value);
      }
      const rows = await this.#db.values(
        `SELECT ${COLUMNS} FROM restate_langgraph_checkpoints ${where.length ? `WHERE ${where.join(' AND ')}` : ''} ORDER BY checkpoint_id DESC`,
        params,
      );
      const wanted = Object.fromEntries(Object.entries(filter ?? {}).filter(([, value]) => value !== undefined));
      const found: CheckpointTuple[] = [];
      for (const row of rows) {
        if (limit !== undefined && found.length >= limit) break;
        const tuple = await this.#tuple(row as CheckpointRow);
        if (matches(tuple.metadata, wanted)) found.push(tuple);
      }
      return found;
    });
    yield* tuples;
  }

  put(
    config: RunnableConfig,
    checkpoint: Checkpoint,
    metadata: CheckpointMetadata,
    newVersions: ChannelVersions,
  ): Promise<RunnableConfig> {
    return this.#serial(async () => {
      const { thread_id, checkpoint_ns = '', checkpoint_id: parentId } = config.configurable ?? {};
      if (!thread_id) throw new Error('Missing "thread_id" field in passed "config.configurable".');
      const { channel_values: values, ...rest } = copyCheckpoint(checkpoint);
      const [type, stored] = await this.serde.dumpsTyped(rest);
      const [metadataType, metadataValue] = await this.serde.dumpsTyped(metadata);
      if (type !== metadataType) throw new Error('Failed to serialize checkpoint and metadata to the same type.');
      // The channels this step changed, at their new versions.
      const changed = await Promise.all(
        Object.entries(newVersions ?? {}).map(async ([channel, version]): Promise<SqlValue[]> => {
          const [valueType, value] = channel in values ? await this.serde.dumpsTyped(values[channel]) : [EMPTY, null];
          return [channel, String(version), valueType, value];
        }),
      );
      await this.#db.transaction(async (tx) => {
        for (const row of changed) {
          await tx.execute(
            'INSERT OR REPLACE INTO restate_langgraph_channel_values (thread_id, checkpoint_ns, channel, version, type, value) VALUES (?, ?, ?, ?, ?, ?)',
            [thread_id, checkpoint_ns, ...row],
          );
        }
        await tx.execute(
          `INSERT OR REPLACE INTO restate_langgraph_checkpoints (${COLUMNS}) VALUES (?, ?, ?, ?, ?, ?, ?)`,
          [thread_id, checkpoint_ns, checkpoint.id, parentId ?? null, type, stored, metadataValue],
        );
        if (this.#latest) await dropEarlier(tx, thread_id, checkpoint_ns, checkpoint);
      });
      return { configurable: { thread_id, checkpoint_ns, checkpoint_id: checkpoint.id } };
    });
  }

  putWrites(config: RunnableConfig, writes: PendingWrite[], taskId: string): Promise<void> {
    return this.#serial(async () => {
      const { thread_id, checkpoint_ns = '', checkpoint_id } = config.configurable ?? {};
      if (!thread_id) throw new Error('Missing thread_id field in config.configurable.');
      if (!checkpoint_id) throw new Error('Missing checkpoint_id field in config.configurable.');
      // Special writes (errors, interrupts) replace; others keep the first.
      const verb = writes.every(([channel]) => channel in WRITES_IDX_MAP) ? 'INSERT OR REPLACE' : 'INSERT OR IGNORE';
      const rows = await Promise.all(
        writes.map(async ([channel, value], idx): Promise<SqlValue[]> => {
          const [type, stored] = await this.serde.dumpsTyped(value);
          return [
            thread_id,
            checkpoint_ns,
            checkpoint_id,
            taskId,
            WRITES_IDX_MAP[channel] ?? idx,
            channel,
            type,
            stored,
          ];
        }),
      );
      await this.#db.transaction(async (tx) => {
        for (const row of rows) {
          await tx.execute(
            `${verb} INTO restate_langgraph_writes (thread_id, checkpoint_ns, checkpoint_id, task_id, idx, channel, type, value) VALUES (?, ?, ?, ?, ?, ?, ?, ?)`,
            row,
          );
        }
      });
    });
  }

  deleteThread(threadId: string): Promise<void> {
    return this.#serial(() =>
      this.#db.transaction(async (tx) => {
        for (const table of [
          'restate_langgraph_checkpoints',
          'restate_langgraph_channel_values',
          'restate_langgraph_writes',
        ]) {
          await tx.execute(`DELETE FROM ${table} WHERE thread_id = ?`, [threadId]);
        }
      }),
    );
  }
}
