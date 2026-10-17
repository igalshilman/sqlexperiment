// The DeepSeek Harness's session persistence (`ctx.sessionPersistence`) over
// a Restate virtual object's SQL database: each session's header, and its
// event log, one row per event, in order.
//
// The harness writes a session through a handle: `append` accepts a batch,
// and `flush` is the durability barrier. Here an append is kept in memory and
// a flush stores everything appended before it in one SQL transaction; a new
// session's header is stored with its first flush, so a session that crashed
// before then never existed. The session checkpoint policy
// (dsh-session-checkpoint-policy) flushes before each model request, before
// each tool body, and before each step; so the events a tool's call follows
// from are stored before the tool runs, and a crash loses only events that
// nothing has acted on.
import type { Context } from '@deepseek-ai/cordis';
import { SessionLogOffset, type SessionEvent, type SessionHeader, type SessionId } from '@deepseek-ai/dsh-session';
import {
  SessionAlreadyExistsError,
  SessionAlreadyOwnedError,
  SessionHandleClosedError,
  SessionPersistence,
  SessionPersistenceNotFoundError,
  SessionPersistenceRevision,
  SessionReadOnlyError,
  assertContiguous,
  materializeAppendBatch,
  materializeCreateHeader,
  validateStoredEvents,
  type SessionAccess,
  type SessionHandle,
  type SessionHandleReadResult,
  type SessionPersistenceCreateOptions,
  type SessionPersistenceSnapshot,
} from '@deepseek-ai/dsh-session-persistence';
import type { SqlSession } from '@restatedev/restate-sdk';

const SCHEMA = `
CREATE TABLE IF NOT EXISTS dsh_sessions (id TEXT PRIMARY KEY, header TEXT NOT NULL, inherited INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS dsh_events (session TEXT NOT NULL, seq INTEGER NOT NULL, event TEXT NOT NULL, PRIMARY KEY (session, seq));
`;

/** A session as stored: its header, inherited prefix length and event count. */
interface Stored {
  header: SessionHeader;
  inherited: number;
  count: number;
}

export class SqlSessionPersistence extends SessionPersistence {
  // Plain fields, not private ones: cordis hands out the service through a proxy.
  db: SqlSession;
  schema: Promise<void> | undefined;
  queue: Promise<unknown> = Promise.resolve();
  /** The open write handle of each session, by id. */
  writers = new Map<SessionId, SqlSessionHandle>();
  /** Sessions created and not stored yet: their headers and inherited counts, by id. */
  created = new Map<SessionId, Omit<Stored, 'count'>>();
  handles = new Set<SqlSessionHandle>();
  abandoned = false;

  /** `config.db`: the object's database (`ctx.db`). */
  constructor(ctx: Context, config: { db: SqlSession }) {
    super(ctx);
    this.db = config.db;
    // The live session's events go to its write handle; its flushes and its
    // disposal reach the handle too.
    ctx.on('session/event', (session, event) => this.writers.get(session.id)?.enqueue(event));
    ctx.on('session/flush', (session) => this.writers.get(session.id)?.flush());
    ctx.on('session/disposed', (session) => void this.writers.get(session.id)?.close());
    ctx.effect(
      () => async () => {
        await Promise.all([...this.handles].map((handle) => handle.close()));
      },
      'sql session handles',
    );
  }

  /**
   * Store nothing more: what is not stored yet is lost, as in a crash. For an
   * attempt that fails, so that its retry resumes from the last checkpoint.
   */
  abandon() {
    this.abandoned = true;
  }

  /** Run `operation` on the database after the ones before it: it serves one statement at a time. */
  serial<T>(operation: (db: SqlSession) => Promise<T>): Promise<T> {
    if (this.abandoned) return Promise.reject(new Error('sql session persistence: abandoned'));
    this.schema ??= this.db.execute(SCHEMA);
    const result = this.queue.then(() => this.schema).then(() => operation(this.db));
    this.queue = result.catch(() => {});
    return result;
  }

  async stored(id: SessionId): Promise<Stored | undefined> {
    const created = this.created.get(id);
    if (created) return { ...created, count: 0 };
    const [row] = await this.serial((db) =>
      db.query(
        'SELECT header, inherited, (SELECT count(*) FROM dsh_events WHERE session = id) FROM dsh_sessions WHERE id = ?',
        [id],
      ),
    );
    return row && { header: JSON.parse(String(row[0])), inherited: Number(row[1]), count: Number(row[2]) };
  }

  async create(header: SessionHeader, options: SessionPersistenceCreateOptions = {}): Promise<SessionHandle> {
    const meta = materializeCreateHeader(header);
    if (await this.stored(meta.id)) throw new SessionAlreadyExistsError(meta.id);
    const inherited = options.inheritedEventCount ?? 0;
    this.created.set(meta.id, { header: meta, inherited });
    return this.openHandle(meta, inherited, 0, 'write');
  }

  async open(id: SessionId, access: SessionAccess): Promise<SessionHandle> {
    const stored = await this.stored(id);
    if (!stored) throw new SessionPersistenceNotFoundError(id);
    if (access === 'write' && this.writers.has(id)) throw new SessionAlreadyOwnedError(id);
    return this.openHandle(stored.header, stored.inherited, stored.count, access);
  }

  openHandle(header: SessionHeader, inherited: number, count: number, access: SessionAccess): SqlSessionHandle {
    const handle: SqlSessionHandle = new SqlSessionHandle(this, header, inherited, count, access, () => {
      this.handles.delete(handle);
      if (this.writers.get(handle.id) === handle) this.writers.delete(handle.id);
    });
    this.handles.add(handle);
    if (access === 'write') this.writers.set(handle.id, handle);
    return handle;
  }

  async flush(): Promise<void> {
    const results = await Promise.allSettled([...this.writers.values()].map((writer) => writer.flush()));
    const errors = results.flatMap((r) => (r.status === 'rejected' ? [r.reason] : []));
    if (errors.length > 0) throw new AggregateError(errors, 'sql session persistence: flush failed');
  }

  async stat(id: SessionId): Promise<SessionPersistenceSnapshot | undefined> {
    const stored = await this.stored(id);
    return stored && snapshot(stored);
  }

  async list(): Promise<readonly SessionPersistenceSnapshot[]> {
    const rows = await this.serial((db) =>
      db.query('SELECT header, inherited, (SELECT count(*) FROM dsh_events WHERE session = id) FROM dsh_sessions'),
    );
    return [
      ...[...this.created.values()].map((created) => snapshot({ ...created, count: 0 })),
      ...rows.map(([header, inherited, count]) =>
        snapshot({ header: JSON.parse(String(header)), inherited: Number(inherited), count: Number(count) }),
      ),
    ];
  }
}

// The log is append-only, so its length identifies its revision.
const snapshot = ({ header, count }: Stored): SessionPersistenceSnapshot => ({
  header,
  revision: SessionPersistenceRevision(`${header.id}:${count}`),
  eventCount: count,
});

/** One open session: its stored events, and for a write handle the ones appended and not yet stored. */
class SqlSessionHandle implements SessionHandle {
  readonly id: SessionId;
  readonly header: SessionHeader;
  readonly inheritedEventCount: SessionLogOffset;
  readonly access: SessionAccess;
  #store: SqlSessionPersistence;
  #release: () => void;
  #stored: number; // events stored
  #pending: SessionEvent[] = []; // appended, not stored yet
  #flushing: Promise<void> = Promise.resolve();
  #closed = false;

  constructor(
    store: SqlSessionPersistence,
    header: SessionHeader,
    inherited: number,
    stored: number,
    access: SessionAccess,
    release: () => void,
  ) {
    this.#store = store;
    this.header = header;
    this.id = header.id;
    this.inheritedEventCount = SessionLogOffset(inherited);
    this.access = access;
    this.#stored = stored;
    this.#release = release;
  }

  #check(operation: string) {
    if (this.#closed) throw new SessionHandleClosedError(this.id, operation);
  }

  /** The next seq: after every stored and appended event. */
  get #end() {
    return this.#stored + this.#pending.length;
  }

  async read(offset = 0, length = Infinity): Promise<SessionHandleReadResult> {
    this.#check('read');
    const rows = await this.#store.serial((db) =>
      db.query('SELECT event FROM dsh_events WHERE session = ? AND seq >= ? ORDER BY seq', [this.id, offset]),
    );
    let events: SessionEvent[] = [
      ...rows.map(([event]) => JSON.parse(String(event)) as SessionEvent),
      ...this.#pending.filter((e) => e.seq >= offset),
    ];
    if (offset === 0) events = validateStoredEvents(this.header, events);
    return { eventState: 'detached', events: events.slice(0, length) };
  }

  async append(events: readonly SessionEvent[]): Promise<void> {
    this.#check('append');
    if (this.access !== 'write') throw new SessionReadOnlyError(this.id, 'append');
    const batch = materializeAppendBatch(events);
    assertContiguous(this.id, batch, this.#end);
    this.#pending.push(...batch);
  }

  /** A live session's event, routed here by the persistence service. */
  enqueue(event: SessionEvent) {
    if (this.#closed || event.seq < this.#end) return;
    this.#pending.push(...materializeAppendBatch([event]));
  }

  /** Store every event appended so far, in one transaction. */
  flush(): Promise<void> {
    this.#check('flush');
    if (this.access !== 'write') throw new SessionReadOnlyError(this.id, 'flush');
    this.#flushing = this.#flushing.then(() => this.#write());
    return this.#flushing;
  }

  /** Store the events appended so far, and the session's header the first time, in one transaction. */
  async #write(): Promise<void> {
    const batch = this.#pending.slice();
    const created = this.#store.created.get(this.id);
    if (batch.length === 0 && !created) return;
    await this.#store.serial((db) =>
      db.transaction(async (tx) => {
        if (created) {
          await tx.execute('INSERT INTO dsh_sessions (id, header, inherited) VALUES (?, ?, ?)', [
            this.id,
            JSON.stringify(created.header),
            created.inherited,
          ]);
        }
        for (const event of batch) {
          await tx.execute('INSERT INTO dsh_events (session, seq, event) VALUES (?, ?, ?)', [
            this.id,
            event.seq,
            JSON.stringify(event),
          ]);
        }
      }),
    );
    if (created) this.#store.created.delete(this.id);
    this.#pending.splice(0, batch.length);
    this.#stored += batch.length;
  }

  async close(): Promise<void> {
    if (this.#closed) return;
    try {
      if (this.access === 'write') await this.flush().catch(() => {});
    } finally {
      this.#closed = true;
      // A session never stored is gone with its writer.
      if (this.access === 'write') this.#store.created.delete(this.id);
      this.#release();
    }
  }

  [Symbol.asyncDispose]() {
    return this.close();
  }
}
