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
// got to: the next conversation's ids and cursors go on from there
// (`START`), so a client following it reads on, and drops the messages
// before `first`.
import type { SqlSession } from 'restate-objects';
import { Schema } from 'restate-objects/internal';
import type { ChatMessage, Messages, MessagesRequest } from './common.ts';

const TABLE = 'restate_agent_log';

/** Where the log starts: past the ids and cursors of the conversations `clear` forgot. */
export interface Start {
  /** The cursor it starts at. */
  seq: number;
  /** Its first message's id. */
  id: number;
}

/** The agent's state key of its log's {@link Start}, which `clear` sets: none before. */
export const START = 'restate-agents.log.start';

/** A message as the transcript gives it: its role and parts, without an id. */
export type Said = Omit<ChatMessage, 'id'>;

const SCHEMA = new Schema('restate-agents.log', [
  `CREATE TABLE ${TABLE} (
    id INTEGER PRIMARY KEY,
    seq INTEGER NOT NULL UNIQUE,
    position INTEGER UNIQUE,
    message TEXT NOT NULL
  )`,
]);

/**
 * Store the transcript's `messages`, in order, in the log: those that
 * changed since the last time, in one transaction.
 */
export async function sync(db: SqlSession, messages: Said[], start: () => Promise<Start | null>) {
  await SCHEMA.ensure(db);
  const head = (await db.query(`SELECT id, message FROM ${TABLE} WHERE position IS NOT NULL ORDER BY position`)).map(
    ([id, message]) => ({ id: Number(id), message: JSON.parse(String(message)) as Said }),
  );
  const both = Math.min(head.length, messages.length);
  let same = 0;
  while (same < both && JSON.stringify(head[same]!.message) === JSON.stringify(messages[same])) same += 1;
  let grown = same;
  while (grown < both && grownFrom(messages[grown]!, head[grown]!.message)) grown += 1;
  if (same === head.length && same === messages.length) return;
  const [[last, next]] = (await db.query(`SELECT MAX(seq), MAX(id) + 1 FROM ${TABLE}`)) as [
    [number | null, number | null],
  ];
  const from = last === null ? ((await start()) ?? { seq: 0, id: 0 }) : { seq: last, id: next! };
  let seq = from.seq;
  let id = from.id;
  await db.transaction(async (tx) => {
    for (let position = same; position < grown; position += 1) {
      await tx.execute(`UPDATE ${TABLE} SET seq = ?, message = ? WHERE id = ?`, [
        (seq += 1),
        JSON.stringify(messages[position]),
        head[position]!.id,
      ]);
    }
    // The rest of the transcript as it was is replaced: its messages stay in the log, out of it.
    await tx.execute(`UPDATE ${TABLE} SET position = NULL WHERE position >= ?`, [grown]);
    for (let position = grown; position < messages.length; position += 1) {
      await tx.execute(`INSERT INTO ${TABLE} VALUES (?, ?, ?, ?)`, [
        id++,
        (seq += 1),
        position,
        JSON.stringify(messages[position]),
      ]);
    }
  });
}

/**
 * A page of the log. With `cursor`: the messages new or changed since it,
 * by when they changed, and the cursor to read on from. Without: the latest
 * `limit` messages (or those before `before`, an id), oldest first, and the
 * cursor to follow the conversation from.
 */
export async function page(
  db: SqlSession,
  { limit = 50, before, cursor }: MessagesRequest,
  start: Start | null,
): Promise<Messages> {
  await SCHEMA.ensure(db);
  const first = start ? { first: start.id } : {};
  if (cursor !== undefined) {
    const rows = await db.query(`SELECT id, message, seq FROM ${TABLE} WHERE seq > ? ORDER BY seq LIMIT ?`, [
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
  const [[now]] = (await db.query(`SELECT COALESCE(MAX(seq), ?) FROM ${TABLE}`, [start?.seq ?? 0])) as [[number]];
  const rows = await db.query(`SELECT id, message FROM ${TABLE} WHERE id < ? ORDER BY id DESC LIMIT ?`, [
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
