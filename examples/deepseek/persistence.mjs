// The DeepSeek Harness's session persistence (`ctx.sessionPersistence`) over
// a Restate virtual object's SQL database: each session's header, and its
// event log, one row per event, in order.
//
// The harness writes a session through a handle: `append` accepts a batch,
// and `flush` is the durability barrier. Here an append is kept in memory and
// a flush stores everything appended before it in one SQL transaction; a new
// session's header is stored with its first flush, so a session that crashed
// before then never existed. The
// session checkpoint policy (dsh-session-checkpoint-policy) flushes before
// each model request, before each tool body, and before each step; so the
// events a tool's call follows from are stored before the tool runs, and a
// crash loses only events that nothing has acted on.
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
} from '@deepseek-ai/dsh-session-persistence';

const SCHEMA = `
CREATE TABLE IF NOT EXISTS dsh_sessions (id TEXT PRIMARY KEY, header TEXT NOT NULL, inherited INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS dsh_events (session TEXT NOT NULL, seq INTEGER NOT NULL, event TEXT NOT NULL, PRIMARY KEY (session, seq));
`;

export class SqlSessionPersistence extends SessionPersistence {
  // Plain fields, not private ones: cordis hands out the service through a proxy.
  _db;
  _ready;
  _tail = Promise.resolve();
  /** The open write handle of each session, by id. */
  _writers = new Map();
  /** Sessions created and not stored yet: their headers and inherited counts, by id. */
  _created = new Map();
  _handles = new Set();
  _abandoned = false;

  /** `config.db`: the object's database (the `db` of `ctx.sqlSession`). */
  constructor(ctx, config) {
    super(ctx);
    this._db = config.db;
    // The live session's events go to its write handle; its flushes and its
    // disposal reach the handle too.
    ctx.on('session/event', (session, event) => this._writers.get(session.id)?.enqueue(event));
    ctx.on('session/flush', (session) => this._writers.get(session.id)?.flush());
    ctx.on('session/disposed', (session) => void this._writers.get(session.id)?.close());
    ctx.effect(() => () => Promise.all([...this._handles].map((handle) => handle.close())), 'sql session handles');
  }

  /**
   * Store nothing more: what is not stored yet is lost, as in a crash. For an
   * attempt that fails, so that its retry resumes from the last checkpoint.
   */
  abandon() {
    this._abandoned = true;
  }

  /** Run `operation` on the database after the ones before it: it serves one statement at a time. */
  serial(operation) {
    if (this._abandoned) return Promise.reject(new Error('sql session persistence: abandoned'));
    this._ready ??= this._db.execute(SCHEMA);
    const result = this._tail.then(() => this._ready).then(() => operation(this._db));
    this._tail = result.catch(() => {});
    return result;
  }

  async _stored(id) {
    const created = this._created.get(id);
    if (created) return { ...created, count: 0 };
    const [row] = await this.serial((db) =>
      db.query(
        'SELECT header, inherited, (SELECT count(*) FROM dsh_events WHERE session = id) FROM dsh_sessions WHERE id = ?',
        [id],
      ),
    );
    return row && { header: JSON.parse(row[0]), inherited: row[1], count: row[2] };
  }

  async create(header, options = {}) {
    const meta = materializeCreateHeader(header);
    if (await this._stored(meta.id)) throw new SessionAlreadyExistsError(meta.id);
    const inherited = options.inheritedEventCount ?? 0;
    this._created.set(meta.id, { header: meta, inherited });
    return this._open(meta, inherited, 0, 'write');
  }

  async open(id, access) {
    const stored = await this._stored(id);
    if (!stored) throw new SessionPersistenceNotFoundError(id);
    if (access === 'write' && this._writers.has(id)) throw new SessionAlreadyOwnedError(id);
    return this._open(stored.header, stored.inherited, stored.count, access);
  }

  _open(header, inherited, count, access) {
    const handle = new SqlSessionHandle(this, header, inherited, count, access, () => {
      this._handles.delete(handle);
      if (this._writers.get(handle.id) === handle) this._writers.delete(handle.id);
    });
    this._handles.add(handle);
    if (access === 'write') this._writers.set(handle.id, handle);
    return handle;
  }

  async flush() {
    const results = await Promise.allSettled([...this._writers.values()].map((writer) => writer.flush()));
    const errors = results.filter((r) => r.status === 'rejected').map((r) => r.reason);
    if (errors.length > 0) throw new AggregateError(errors, 'sql session persistence: flush failed');
  }

  async stat(id) {
    const stored = await this._stored(id);
    return stored && snapshot(stored);
  }

  async list() {
    const rows = await this.serial((db) =>
      db.query(
        'SELECT header, inherited, (SELECT count(*) FROM dsh_events WHERE session = id) FROM dsh_sessions',
      ),
    );
    return [
      ...[...this._created.values()].map((created) => snapshot({ ...created, count: 0 })),
      ...rows.map(([header, inherited, count]) => snapshot({ header: JSON.parse(header), inherited, count })),
    ];
  }
}

// The log is append-only, so its length identifies its revision.
const snapshot = ({ header, count }) => ({
  header,
  revision: SessionPersistenceRevision(`${header.id}:${count}`),
  eventCount: count,
});

/** One open session: its stored events, and for a write handle the ones appended and not yet stored. */
class SqlSessionHandle {
  #store;
  #release;
  #stored; // events stored
  #pending = []; // appended, not stored yet
  #flushing = Promise.resolve();
  #closed = false;

  constructor(store, header, inherited, stored, access, release) {
    this.#store = store;
    this.header = header;
    this.id = header.id;
    this.inheritedEventCount = inherited;
    this.access = access;
    this.#stored = stored;
    this.#release = release;
  }

  #check(operation) {
    if (this.#closed) throw new SessionHandleClosedError(this.id, operation);
  }

  /** The next seq: after every stored and appended event. */
  get #end() {
    return this.#stored + this.#pending.length;
  }

  async read(offset = 0, length = Infinity) {
    this.#check('read');
    const rows = await this.#store.serial((db) =>
      db.query('SELECT event FROM dsh_events WHERE session = ? AND seq >= ? ORDER BY seq', [this.id, offset]),
    );
    let events = [...rows.map(([event]) => JSON.parse(event)), ...this.#pending.filter((e) => e.seq >= offset)];
    if (offset === 0) events = validateStoredEvents(this.header, events);
    return { eventState: 'detached', events: events.slice(0, length) };
  }

  async append(events) {
    this.#check('append');
    if (this.access !== 'write') throw new SessionReadOnlyError(this.id, 'append');
    const batch = materializeAppendBatch(events);
    assertContiguous(this.id, batch, this.#end);
    this.#pending.push(...batch);
  }

  /** A live session's event, routed here by the persistence service. */
  enqueue(event) {
    if (this.#closed || event.seq < this.#end) return;
    this.#pending.push(...materializeAppendBatch([event]));
  }

  /** Store every event appended so far, in one transaction. */
  flush() {
    this.#check('flush');
    if (this.access !== 'write') throw new SessionReadOnlyError(this.id, 'flush');
    this.#flushing = this.#flushing.then(async () => {
      const batch = this.#pending.slice();
      const created = this.#store._created.get(this.id);
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
      if (created) this.#store._created.delete(this.id);
      this.#pending.splice(0, batch.length);
      this.#stored += batch.length;
    });
    return this.#flushing;
  }

  async close() {
    if (this.#closed) return;
    try {
      if (this.access === 'write') await this.flush().catch(() => {});
    } finally {
      this.#closed = true;
      // A session never stored is gone with its writer.
      if (this.access === 'write') this.#store._created.delete(this.id);
      this.#release();
    }
  }

  [Symbol.asyncDispose]() {
    return this.close();
  }
}
