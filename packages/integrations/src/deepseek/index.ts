// DeepSeek Harness agents in Restate virtual objects: the session (the
// harness's event log) is in the object's database (persistence.ts).
//
//   handlers: { chat: deepseek.chat({ tools: [weather] }) }
//
// The model is DeepSeek's (@deepseek-ai/dsh-llm-deepseek-api-key): `model`,
// or else DEEPSEEK_MODEL (default deepseek-flash), with DEEPSEEK_API_KEY.
//
// The handler is not replayed: a retried attempt (a crash, a lost connection)
// boots the harness again and resumes the session from the stored log.
// - The harness's session checkpoint policy stores the log before each model
//   request and before each tool body: a tool runs only once its call is
//   stored.
// - A crash leaves the turn open. Resuming closes it, and gives each tool
//   call that was stored without a result an error result saying its outcome
//   is unknown; the handler then asks the agent to continue. The model sees
//   the unknown outcome: a tool that must not run twice needs a way for the
//   model to name the call it retries (such as a `retryOf` argument).
// - A tool's error fails the attempt, which Restate retries (the harness
//   would hand it to the model as the tool's result).
// - A message is answered once per invocation: its message carries the
//   invocation id, and a retried attempt returns the answer of the turn that
//   finished it.
import type * as restate from '@restatedev/restate-sdk';
import { Context } from '@deepseek-ai/cordis';
import { AgentRegistry } from '@deepseek-ai/dsh-agent';
import { AgentLoop } from '@deepseek-ai/dsh-agent-loop';
import * as deepseek from '@deepseek-ai/dsh-llm-deepseek-api-key';
import { LlmRuntime, MessageId, freezeMessage, type Message, type UserMessage } from '@deepseek-ai/dsh-llm';
import { SessionId, SessionStore, type SessionEvent } from '@deepseek-ai/dsh-session';
import * as checkpoints from '@deepseek-ai/dsh-session-checkpoint-policy';
import { SessionProjectionRegistry } from '@deepseek-ai/dsh-session-projection';
import { SystemPrompt } from '@deepseek-ai/dsh-system-prompt';
import { ToolRuntime, type ToolDefinition } from '@deepseek-ai/dsh-tools';
import { forCall, type PerCall } from '../common.ts';
import { SqlSessionPersistence } from './persistence.ts';

export { SqlSessionPersistence };

/** The default model. */
export const DEFAULT_MODEL = 'deepseek-flash';

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
async function boot(db: restate.SqlSession, system: string): Promise<Context> {
  const harness = new Context();
  await harness.plugin(LlmRuntime);
  await harness.plugin(SessionStore);
  await harness.plugin(SystemPrompt, { personaPrefix: system });
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

export interface ChatOptions {
  /** The model: DEEPSEEK_MODEL, or else {@link DEFAULT_MODEL}. */
  model?: string;
  /** The system prompt's persona. */
  system?: string;
  /** The agent's tools (`defineTool` of @deepseek-ai/dsh-tools), or for an invocation. */
  tools?: PerCall<ToolDefinition>[];
}

/** A chat handler: each message an invocation, the session in the object's database. */
export const chat =
  ({ model = process.env.DEEPSEEK_MODEL ?? DEFAULT_MODEL, system = 'Answer briefly.', tools = [] }: ChatOptions = {}) =>
  async (ctx: restate.SqlObjectContext, text: string): Promise<string> => {
    const id = ctx.request().id;
    const harness = await boot(ctx.db, system);
    // A tool's error fails the attempt, which Restate retries: the harness
    // stores nothing more, so the retry finds the tool's call without a result.
    const { promise: failed, reject: fail } = Promise.withResolvers<never>();
    failed.catch(() => {});
    try {
      const sessionId = SessionId(ctx.key);
      const options = {
        agentOptions: { provider: 'deepseek-official', model },
        setup: (agentCtx: Context) =>
          tools.forEach((t) => {
            const tool = forCall(t, ctx);
            agentCtx.tools.register({ ...tool, execute: (args, exec) => guard(fail, () => tool.execute(args, exec)) });
          }),
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
        if (answer !== undefined) return answer;
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
