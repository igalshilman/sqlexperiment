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
import type { SqlSession } from 'restate-fns';
import type { ChatMessage, Messages, MessagesRequest } from './common.ts';

const TABLE = 'agent_log';

/** A message as the transcript gives it: its role and parts, without an id. */
export type Said = Omit<ChatMessage, 'id'>;

const SCHEMA = `CREATE TABLE IF NOT EXISTS ${TABLE} (
  id INTEGER PRIMARY KEY,
  seq INTEGER NOT NULL UNIQUE,
  position INTEGER UNIQUE,
  message TEXT NOT NULL
)`;

const hasLog = async (db: SqlSession) =>
  (await db.query("SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = ?", [TABLE])).length > 0;

/**
 * Store the transcript's `messages`, in order, in the log: those that
 * changed since the last time, in one transaction.
 */
export async function sync(db: SqlSession, messages: Said[]) {
  if (!(await hasLog(db))) await db.execute(SCHEMA);
  const head = (await db.query(`SELECT id, message FROM ${TABLE} WHERE position IS NOT NULL ORDER BY position`)).map(
    ([id, message]) => ({ id: Number(id), message: JSON.parse(String(message)) as Said }),
  );
  const both = Math.min(head.length, messages.length);
  let same = 0;
  while (same < both && JSON.stringify(head[same]!.message) === JSON.stringify(messages[same])) same += 1;
  let grown = same;
  while (grown < both && grownFrom(messages[grown]!, head[grown]!.message)) grown += 1;
  if (same === head.length && same === messages.length) return;
  const [[last, next]] = (await db.query(`SELECT COALESCE(MAX(seq), 0), COALESCE(MAX(id), -1) + 1 FROM ${TABLE}`)) as [
    [number, number],
  ];
  let seq = last;
  let id = next;
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
export async function page(db: SqlSession, { limit = 50, before, cursor }: MessagesRequest = {}): Promise<Messages> {
  if (!(await hasLog(db))) return { messages: [], more: false, cursor: cursor ?? 0 };
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
    };
  }
  const [[now]] = (await db.query(`SELECT COALESCE(MAX(seq), 0) FROM ${TABLE}`)) as [[number]];
  const rows = await db.query(`SELECT id, message FROM ${TABLE} WHERE id < ? ORDER BY id DESC LIMIT ?`, [
    before ?? Number.MAX_SAFE_INTEGER,
    limit + 1,
  ]);
  return { messages: rows.slice(0, limit).toReversed().map(message), more: rows.length > limit, cursor: now };
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
