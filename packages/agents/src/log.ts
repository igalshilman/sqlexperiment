// An agent's conversation as clients read it: a log in the object's
// database, kept in step with the framework's transcript each time the agent
// publishes a change (`transcribing` in common.ts), and read by `messages`
// and `sync`.
//
// Each message has an `id` that never changes, and a `seq`: the sequence
// number of its last change, which is the cursor. A message that grows in
// place (a tool call's output comes) keeps its id and takes a new `seq`; a
// transcript replaced (compacted, reset) leaves its messages in the log, and
// the messages after them are new ones. So a client reads from a cursor, and
// gets each message that is new or changed since, once.
//
// `clear` forgets the log with the rest of the conversation, but not where it
// got to: the next conversation's ids and cursors go on from there (its
// start, a row of `restate_agent_log_start`), so a client following it reads
// on, and drops the messages before `first`.
import type { Database, DatabaseReader, DatabaseWriter } from 'restate-sessions';
import { Schema } from 'restate-sessions/internal';
import type { ChatMessage, Messages, MessagesRequest } from './common.ts';

const TABLE = 'restate_agent_log';

/** Where the log starts: past the ids and cursors of the conversations `clear` forgot. */
export interface Start {
  /** The cursor it starts at. */
  seq: number;
  /** Its first message's id. */
  id: number;
}

/** A message as the transcript gives it: its role and parts, without an id. */
export type Said = Omit<ChatMessage, 'id'>;

const SCHEMA = new Schema('restate-agents.log', [
  `CREATE TABLE ${TABLE} (
    id INTEGER PRIMARY KEY,
    seq INTEGER NOT NULL UNIQUE,
    position INTEGER UNIQUE,
    message TEXT NOT NULL
  )`,
  // Where the log starts, which `clear` sets: no row before.
  `CREATE TABLE ${TABLE}_start (
    row INTEGER PRIMARY KEY CHECK (row = 0),
    seq INTEGER NOT NULL,
    id INTEGER NOT NULL
  )`,
]);

/** Where the log of `db` starts, if `clear` set it. */
const startOf = async (db: DatabaseReader): Promise<Start | undefined> =>
  db.get<Start>(`SELECT seq, id FROM ${TABLE}_start`);

/**
 * Start the log at `start`, in the transaction `tx` of `clear` that emptied
 * the database: the next conversation's ids and cursors go on from there.
 */
export async function restart(tx: DatabaseWriter, start: Start) {
  await tx.execute(`INSERT OR REPLACE INTO ${TABLE}_start (row, seq, id) VALUES (0, ?, ?)`, [start.seq, start.id]);
}

/** A message of the log's head, the transcript as last stored: its id, and the message, also as JSON. */
interface Row {
  id: number;
  message: Said;
  json: string;
}

/** The log's head as a session stored it last, with the last cursor and the next id. */
interface Head {
  rows: Row[];
  seq: number;
  next: number;
}

/**
 * The head of each session (`ctx.db`, one per attempt), as its last `sync`
 * stored it: the next compares the transcript with it, rather than read the
 * whole log again at each step.
 */
const heads = new WeakMap<Database, Head>();

/** The log's head in `db`: as this session stored it last, or else as the log has it. */
async function headOf(db: Database): Promise<Head> {
  const cached = heads.get(db);
  if (cached) return cached;
  const rows = (await db.values(`SELECT id, message FROM ${TABLE} WHERE position IS NOT NULL ORDER BY position`)).map(
    ([id, json]) => ({ id: Number(id), message: JSON.parse(String(json)) as Said, json: String(json) }),
  );
  const { last, next } = (await db.get<{ last: number | null; next: number | null }>(
    `SELECT MAX(seq) AS last, MAX(id) + 1 AS next FROM ${TABLE}`,
  ))!;
  if (last !== null) return { rows, seq: last, next: next! };
  const from = (await startOf(db)) ?? { seq: 0, id: 0 };
  return { rows, seq: from.seq, next: from.id };
}

/**
 * Store the transcript's `messages`, in order, in the log: those that
 * changed since the last time, in one transaction.
 */
export async function sync(db: Database, messages: Said[]) {
  await SCHEMA.ensure(db);
  const head = await headOf(db);
  // A message the transcript did not change is the one stored (see Transcript in common.ts):
  // only the others are compared, as JSON.
  const jsons: string[] = [];
  const jsonOf = (position: number) => (jsons[position] ??= JSON.stringify(messages[position]));
  const unchanged = (position: number) => {
    const row = head.rows[position]!;
    return row.message === messages[position] || row.json === jsonOf(position);
  };
  const both = Math.min(head.rows.length, messages.length);
  let same = 0;
  while (same < both && unchanged(same)) same += 1;
  let grown = same;
  while (grown < both && grownFrom(messages[grown]!, head.rows[grown]!.message)) grown += 1;
  if (same === head.rows.length && same === messages.length) return;
  let { seq, next: id } = head;
  const rows = head.rows.slice(0, same);
  try {
    await db.transaction(async (tx) => {
      for (let position = same; position < grown; position += 1) {
        const row = { id: head.rows[position]!.id, message: messages[position]!, json: jsonOf(position) };
        await tx.execute(`UPDATE ${TABLE} SET seq = ?, message = ? WHERE id = ?`, [(seq += 1), row.json, row.id]);
        rows.push(row);
      }
      // The rest of the transcript as it was is replaced: its messages stay in the log, out of it.
      await tx.execute(`UPDATE ${TABLE} SET position = NULL WHERE position >= ?`, [grown]);
      for (let position = grown; position < messages.length; position += 1) {
        const row = { id: id++, message: messages[position]!, json: jsonOf(position) };
        await tx.execute(`INSERT INTO ${TABLE} VALUES (?, ?, ?, ?)`, [row.id, (seq += 1), position, row.json]);
        rows.push(row);
      }
    });
  } catch (error) {
    // Rolled back: the log is as it was, which the next sync reads again.
    heads.delete(db);
    throw error;
  }
  heads.set(db, { rows, seq, next: id });
}

/**
 * A page of the log. With `cursor`: the messages new or changed since it,
 * by when they changed, and the cursor to read on from. Without: the latest
 * `limit` messages (or those before `before`, an id), oldest first, and the
 * cursor to follow the conversation from.
 */
export async function page(db: DatabaseReader, { limit = 50, before, cursor }: MessagesRequest): Promise<Messages> {
  await SCHEMA.ensure(db);
  const start = await startOf(db);
  const first = start ? { first: start.id } : {};
  if (cursor !== undefined) {
    const rows = await db.values(`SELECT id, message, seq FROM ${TABLE} WHERE seq > ? ORDER BY seq LIMIT ?`, [
      cursor,
      limit + 1,
    ]);
    const shown = rows.slice(0, limit);
    return {
      messages: shown.map(message),
      more: rows.length > limit,
      cursor: shown.length > 0 ? Number(shown.at(-1)![2]) : cursor,
      ...first,
    };
  }
  const { now } = (await db.get<{ now: number }>(`SELECT COALESCE(MAX(seq), ?) AS now FROM ${TABLE}`, [
    start?.seq ?? 0,
  ]))!;
  const rows = await db.values(`SELECT id, message FROM ${TABLE} WHERE id < ? ORDER BY id DESC LIMIT ?`, [
    before ?? Number.MAX_SAFE_INTEGER,
    limit + 1,
  ]);
  return {
    messages: rows.slice(0, limit).toReversed().map(message),
    more: rows.length > limit,
    cursor: now,
    ...first,
  };
}

/** A row of the log, as `messages` gives it. */
const message = ([id, m]: unknown[]): ChatMessage => ({ id: Number(id), ...(JSON.parse(String(m)) as Said) });

/** Whether `now` is `was` grown in place: the same role and parts, with more (a tool call's output, say). */
function grownFrom(now: Said, was: Said) {
  if (was.role !== now.role || now.parts.length < was.parts.length) return false;
  return was.parts.every((part, i) => {
    const other = now.parts[i]!;
    if (part.type !== other.type) return false;
    if (part.type === 'text') return other.type === 'text' && other.text.startsWith(part.text);
    return other.type === 'tool' && other.id === part.id;
  });
}
