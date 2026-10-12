// A conversation with Claude in a Restate virtual object's database: its
// messages, in order, each with the id of the prompt it belongs to, and the
// answer to each prompt.
//
// It is the only state of the tool runner (@anthropic-ai/sdk's agent loop),
// which keeps the conversation in its params: an attempt starts a runner with
// the stored messages, and stores each message the runner adds.

import type Anthropic from '@anthropic-ai/sdk';
import type { SqlSession } from '@restatedev/restate-sdk';

type MessageParam = Anthropic.Beta.Messages.BetaMessageParam;

const SCHEMA = `
CREATE TABLE IF NOT EXISTS messages (seq INTEGER PRIMARY KEY, prompt TEXT NOT NULL, message TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS answers (prompt TEXT PRIMARY KEY, answer TEXT NOT NULL);
`;

export class Conversation {
  #db: SqlSession;

  constructor(db: SqlSession) {
    this.#db = db;
  }

  /** The conversation in `db` (the `db` of `ctx.sqlSession`). */
  static async open(db: SqlSession): Promise<Conversation> {
    await db.execute(SCHEMA);
    return new Conversation(db);
  }

  /** The answer to prompt `id`, or undefined. */
  async answer(id: string): Promise<string | undefined> {
    const [row] = await this.#db.query('SELECT answer FROM answers WHERE prompt = ?', [id]);
    return row?.[0] as string | undefined;
  }

  /** The messages, in order. */
  async messages(): Promise<MessageParam[]> {
    const rows = await this.#db.query('SELECT message FROM messages ORDER BY seq');
    return rows.map(([message]) => JSON.parse(String(message)));
  }

  /** The prompt being answered: the last message's, unless it is answered. */
  async pending(): Promise<string | undefined> {
    const [row] = await this.#db.query(
      'SELECT prompt FROM messages WHERE prompt NOT IN (SELECT prompt FROM answers) ORDER BY seq DESC LIMIT 1',
    );
    return row?.[0] as string | undefined;
  }

  /** Store `messages` of prompt `id`, and its `answer` if given, in one transaction. */
  append(id: string, messages: MessageParam[], answer?: string): Promise<void> {
    return this.#db.transaction(async (tx) => {
      for (const message of messages) {
        await tx.execute('INSERT INTO messages (prompt, message) VALUES (?, ?)', [id, JSON.stringify(message)]);
      }
      if (answer !== undefined) await tx.execute('INSERT INTO answers (prompt, answer) VALUES (?, ?)', [id, answer]);
    });
  }
}
