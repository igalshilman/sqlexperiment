// A DeepSeek Harness agent in a Restate virtual object: a DeepSeekAgent object
// per conversation, whose session (the harness's event log) is in the
// object's database (persistence.ts). Its tools move money in the ledger
// (../node/ledger.ts).
//
// The handler is not replayed: a retried attempt (a crash, a lost connection)
// boots the harness again and resumes the session from the stored log.
// - The harness's session checkpoint policy stores the log before each model
//   request and before each tool body: a tool runs only once its call is
//   stored.
// - A crash leaves the turn open. Resuming closes it, and gives each tool
//   call that was stored without a result an error result saying its outcome
//   is unknown; the handler then asks the agent to continue. The model sees
//   the unknown outcome: the transfer tool takes `retryOf`, the id of the
//   call to retry, and uses it as the transfer id, so a retried transfer moves
//   the money once.
// - A tool's call to the ledger has an idempotency key made from the request
//   id (`ctx.request().id`, the same on every attempt) and the transfer id,
//   so a retried attempt reaches the call the first one made.
// - A prompt is answered once per prompt id: its message carries the id, and
//   a prompt asked again returns the answer of the turn that finished it.
//
// Environment: DEEPSEEK_API_KEY, DEEPSEEK_MODEL (default deepseek-flash),
// PORT (default 9080) and ABORT_PER_MILLE (see ../faults.ts).
import * as restate from '@restatedev/restate-sdk';
import { Context } from '@deepseek-ai/cordis';
import { AgentRegistry } from '@deepseek-ai/dsh-agent';
import { AgentLoop } from '@deepseek-ai/dsh-agent-loop';
import * as deepseek from '@deepseek-ai/dsh-llm-deepseek-api-key';
import { LlmRuntime, MessageId, freezeMessage, type Message, type UserMessage } from '@deepseek-ai/dsh-llm';
import { SessionId, SessionStore, type SessionEvent } from '@deepseek-ai/dsh-session';
import * as checkpoints from '@deepseek-ai/dsh-session-checkpoint-policy';
import { SessionProjectionRegistry } from '@deepseek-ai/dsh-session-projection';
import { SystemPrompt } from '@deepseek-ai/dsh-system-prompt';
import { ToolRuntime, defineTool } from '@deepseek-ai/dsh-tools';
import { serveIfMain } from '../faults.ts';
import { ledger } from '../node/ledger.ts';
import { SqlSessionPersistence } from './persistence.ts';

/**
 * The bank tools of one invocation: they reach the ledger through `ctx`. A
 * tool's error goes to `fail` (the harness would hand it to the model as the
 * tool's result).
 */
const bank = (ctx: restate.SqlObjectContext, fail: (error: unknown) => void) => {
  const books = ctx.objectClient(ledger, 'bank');
  const key = (id: string) => `${ctx.request().id}:${id}`;
  return [
    defineTool({
      name: 'transfer',
      description:
        "Move an amount of money from one account to another. If an earlier transfer's outcome is unknown, " +
        'retry it with `retryOf` set to its tool call id: the money then moves once.',
      parameters: {
        from: { type: 'integer', required: true, description: 'Account to take the money from' },
        to: { type: 'integer', required: true, description: 'Account to put the money in' },
        amount: { type: 'integer', required: true },
        retryOf: { type: 'string', description: 'The tool call id of the transfer this one retries' },
      },
      output: { schema: { type: 'string' }, render: (_args, value) => [{ type: 'text', text: value }] },
      execute: ({ from, to, amount, retryOf }, exec) =>
        guard(fail, async () => {
          const callId = retryOf ?? exec.callId;
          const id = `DeepSeekAgent/${ctx.key}/${callId}`; // unique in the ledger
          // What moved: a retry of an earlier call gets that call's transfer.
          const applied = await books.transfer(
            { id, from, to, amount },
            restate.rpc.opts({ idempotencyKey: key(callId) }),
          );
          return `moved ${applied.amount} from account ${applied.from} to account ${applied.to}`;
        }),
    }),
    defineTool({
      name: 'audit',
      description: 'Check the books: accounts, total money, transfers, and inconsistent accounts.',
      parameters: {},
      output: { schema: { type: 'string' }, render: (_args, value) => [{ type: 'text', text: value }] },
      execute: (_args, exec) =>
        guard(fail, async () =>
          JSON.stringify(await books.audit(restate.rpc.opts({ idempotencyKey: key(exec.callId) }))),
        ),
    }),
  ];
};

/** Run a tool's `body`, whose error also goes to `fail`. */
async function guard<T>(fail: (error: unknown) => void, body: () => Promise<T>): Promise<T> {
  try {
    return await body();
  } catch (e) {
    fail(e);
    throw e;
  }
}

/** The harness, its session in `db`: the services an agent loop needs, the persistence and its checkpoints. */
async function boot(db: restate.SqlSession): Promise<Context> {
  const harness = new Context();
  await harness.plugin(LlmRuntime);
  await harness.plugin(SessionStore);
  await harness.plugin(SystemPrompt, {
    personaPrefix: 'You keep the books of a bank, with your tools. Answer briefly.',
  });
  await harness.plugin(ToolRuntime, {});
  await harness.plugin(AgentRegistry);
  await harness.plugin(SessionProjectionRegistry);
  await harness.plugin(SqlSessionPersistence, { db });
  await harness.plugin(checkpoints);
  await harness.plugin(deepseek, {}); // DEEPSEEK_API_KEY
  await harness.plugin(AgentLoop, { agents: [] });
  return harness;
}

/** The text that asks the agent to finish an interrupted prompt. */
const CONTINUE = 'Continue: your last turn was interrupted. Finish the request.';

const promptId = (id: string) => MessageId(`prompt:${id}`);
const textOf = (message: Message) => message.content.flatMap((c) => (c.type === 'text' ? [c.text] : [])).join('');

/**
 * The answer to prompt `id` in the session log: the last reply of the first
 * turn completed after it; undefined while it is absent or unfinished.
 */
function answerOf(events: readonly SessionEvent[], id: MessageId): string | undefined {
  const at = events.findIndex((e) => e.type === 'user/message' && e.data.id === id);
  if (at < 0) return undefined;
  let answer: string | undefined;
  for (const e of events.slice(at)) {
    if (e.type === 'assistant/message') answer = textOf(e.data.message);
    if (e.type === 'turn/end' && e.data.reason.kind === 'completed') return answer ?? '';
  }
  return undefined;
}

/** The last prompt in the session log, by id. */
function lastPrompt(events: readonly SessionEvent[]): MessageId | undefined {
  const prompt = events.findLast(
    (e): e is SessionEvent<'user/message'> => e.type === 'user/message' && e.data.id.startsWith('prompt:'),
  );
  return prompt?.data.id;
}

const message = (id: string, text: string): UserMessage =>
  freezeMessage({ id: MessageId(id), role: 'user', content: [{ type: 'text', text }], source: { kind: 'user' } });

/** Answer prompt `text`, once per prompt `id`. */
export const prompt = async (ctx: restate.SqlObjectContext, { id, text }: { id: string; text: string }) => {
  const db = ctx.db;
  const harness = await boot(db);
  // A tool's error fails the attempt, which Restate retries: the harness
  // stores nothing more, so the retry finds the tool's call without a result.
  const { promise: failed, reject: fail } = Promise.withResolvers<never>();
  failed.catch(() => {});
  try {
    const sessionId = SessionId(ctx.key);
    const options = {
      agentOptions: { provider: 'deepseek-official', model: process.env.DEEPSEEK_MODEL ?? 'deepseek-flash' },
      setup: (agentCtx: Context) => bank(ctx, fail).forEach((tool) => agentCtx.tools.register(tool)),
    };
    // Resuming closes a turn a crash left open.
    const { agent } = (await harness.sessionPersistence.stat(sessionId))
      ? await harness.agents.resume({ resumeSessionId: sessionId, ...options })
      : await harness.agents.create({ sessionId, ...options });
    // The log as this attempt found it: a turn that failed before is
    // continued like an interrupted one.
    const found = agent.session.snapshotEvents().length;
    for (;;) {
      const events = agent.session.snapshotEvents();
      const answer = answerOf(events, promptId(id));
      if (answer !== undefined) return { answer };
      // A turn of this attempt that failed (the model's request, or a
      // checkpoint) fails the attempt. Its end may be stored already, so
      // only this attempt's turns count: the retry asks the model again.
      const end = events.slice(found).findLast((e): e is SessionEvent<'turn/end'> => e.type === 'turn/end')
        ?.data.reason;
      if (end?.kind === 'error') throw new Error(`the agent's turn failed: ${JSON.stringify(end.error)}`);
      // Finish an unfinished prompt first (this one's, after a crash), then
      // start this one.
      const last = lastPrompt(events);
      if (last !== undefined && answerOf(events, last) === undefined) {
        agent.followup(message(`continue:${events.length}`, CONTINUE));
      } else {
        agent.followup(message(promptId(id), text));
      }
      await Promise.race([agent.whenIdle(), failed]);
      await harness.sessions.flush(agent.session);
    }
  } catch (e) {
    (harness.sessionPersistence as SqlSessionPersistence).abandon();
    throw e;
  } finally {
    await harness.fiber.dispose();
  }
};

export const agent = restate.object({
  name: 'DeepSeekAgent',
  handlers: { prompt },
  options: {
    ...restate.sqlOptions,
    // Keep retrying rather than pause after Restate's default number of
    // attempts: each attempt resumes from the stored log.
    retryPolicy: { initialInterval: 50, maxInterval: 500, maxAttempts: 100_000 },
  },
});

serveIfMain(import.meta, [agent, ledger]);
