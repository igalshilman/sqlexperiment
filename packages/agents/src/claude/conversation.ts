// A conversation with Claude in a Restate virtual object's database: its
// messages, in order, each with the id of the prompt it belongs to, and the
// answer to each prompt.
//
// It is the only state of the tool runner (@anthropic-ai/sdk's agent loop),
// which keeps the conversation in its params: an attempt starts a runner with
// the stored messages, and stores each message the runner adds.

import type Anthropic from '@anthropic-ai/sdk';
import type { SqlSession } from 'restate-objects';
import { Schema } from 'restate-objects/internal';

type MessageParam = Anthropic.Beta.Messages.BetaMessageParam;

const SCHEMA = new Schema('restate-agents.claude', [
  'CREATE TABLE restate_claude_messages (seq INTEGER PRIMARY KEY, prompt TEXT NOT NULL, message TEXT NOT NULL)',
  'CREATE TABLE restate_claude_answers (prompt TEXT PRIMARY KEY, answer TEXT NOT NULL)',
]);

/** The conversation in the object's database: the tool runner's messages, and each prompt's answer. */
export class Conversation {
  #db: SqlSession;
  /** Cancelled: a run cut off then stores nothing more. */
  #closed = false;

  constructor(db: SqlSession) {
    this.#db = db;
  }

  /** The conversation in `db` (`ctx.db`). */
  static async open(db: SqlSession): Promise<Conversation> {
    await SCHEMA.ensure(db);
    return new Conversation(db);
  }

  /** The answer to prompt `id`, or undefined. */
  async answer(id: string): Promise<string | undefined> {
    const [row] = await this.#db.query('SELECT answer FROM restate_claude_answers WHERE prompt = ?', [id]);
    return row?.[0] as string | undefined;
  }

  /** The messages, in order. */
  async messages(): Promise<MessageParam[]> {
    const rows = await this.#db.query('SELECT message FROM restate_claude_messages ORDER BY seq');
    return rows.map(([message]) => JSON.parse(String(message)));
  }

  /** The prompt being answered: the last message's, unless it is answered. */
  async pending(): Promise<string | undefined> {
    const [row] = await this.#db.query(
      'SELECT prompt FROM restate_claude_messages WHERE prompt NOT IN (SELECT prompt FROM restate_claude_answers) ORDER BY seq DESC LIMIT 1',
    );
    return row?.[0] as string | undefined;
  }

  /**
   * Close prompt `id`, which was cancelled: a result for each tool call of
   * the last reply that has none, then `text` as the reply and the answer.
   */
  async cancel(id: string, text: string): Promise<void> {
    this.#closed = true;
    await this.close(id, text);
  }

  /**
   * Close prompt `id` with `text` as its last reply and its answer, after
   * `reply` if given (a reply that cannot go on: cut off, or declined): first
   * a result of `text` for each tool call of the last reply that has none.
   */
  async close(id: string, text: string, reply?: MessageParam): Promise<void> {
    const calls = toolUses(reply ?? (await this.messages()).at(-1));
    const results = calls.map((call) => ({
      type: 'tool_result' as const,
      tool_use_id: call.id,
      content: text,
      is_error: true,
    }));
    const closing: MessageParam[] = results.length > 0 ? [{ role: 'user', content: results }] : [];
    await this.#store(id, [...(reply ? [reply] : []), ...closing, { role: 'assistant', content: text }], text);
  }

  /** Store `messages` of prompt `id`, and its `answer` if given, in one transaction. */
  append(id: string, messages: MessageParam[], answer?: string): Promise<void> {
    if (this.#closed) return Promise.reject(new Error('the conversation was cancelled'));
    return this.#store(id, messages, answer);
  }

  #store(id: string, messages: MessageParam[], answer?: string): Promise<void> {
    return this.#db.transaction(async (tx) => {
      for (const message of messages) {
        await tx.execute('INSERT INTO restate_claude_messages (prompt, message) VALUES (?, ?)', [
          id,
          JSON.stringify(message),
        ]);
      }
      if (answer !== undefined)
        await tx.execute('INSERT INTO restate_claude_answers (prompt, answer) VALUES (?, ?)', [id, answer]);
    });
  }
}

/** The tool calls of `message`, if it is the assistant's. */
const toolUses = (message: MessageParam | undefined) =>
  message?.role === 'assistant' && Array.isArray(message.content)
    ? message.content.filter((b) => b.type === 'tool_use')
    : [];
