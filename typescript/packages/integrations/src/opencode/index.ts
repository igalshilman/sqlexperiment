// OpenCode 2.0 agents in Restate virtual objects: OpenCode for each
// invocation, with its database in the object's state.
//
//   const coder = opencode.agent({ name: 'coder', workspace: (ctx) => `/work/${ctx.key}` });
//
// or a handler of an object of your own: `handlers: { chat: opencode.chat() }`,
// with options for each invocation: `opencode.chat(async (ctx) => ({ workspace }))`
//
// or, for more than a prompt:
//
//   review: (ctx, task) => withOpenCode(ctx, { tools }, async ({ prompt }) => [await prompt(task), await prompt('Review it')]),
//
// The model is `model`, MODEL, or else openai/gpt-6-sol,
// which OpenCode's own providers serve, with the provider's API key (such as
// OPENAI_API_KEY).
//
// It runs `work` over a database of the key's own, beside `ctx.db`
// (`databaseOf`): OpenCode's tables are alone there (db.ts), as OpenCode
// boots only on a database that is empty or its own, and each OpenCode
// transaction is an SQL transaction in it, stored in order with everything
// else the handler does. `ctx.db` keeps the conversation's log, and the
// tables of yours.
// OpenCode is booted with `plugins` and a plugin of the `tools`, and closed at
// the end.
//
// A retried attempt (a crash, a lost connection) boots OpenCode again, and
// OpenCode resumes the turn the last attempt left unfinished: it records an
// execution claim when a turn starts and releases it when the turn ends, and
// resumes every claimed turn when it boots. A prompt is submitted once, by its
// id (OpenCode's message id), and an answered prompt returns its answer.
//
// A resumed turn gives a tool call that was running an "interrupted" result
// and asks the model to continue, so a tool that must not run twice needs a
// way for the model to name the call it retries (such as a `retryOf`
// argument). A tool that throws fails the attempt rather than hand the error
// to the model: from then on the attempt stores nothing (its fence closes),
// and the next attempt finds the call interrupted.
import * as restate from 'restate-fns';
import { databaseOf } from 'restate-fns/internal';
import { mkdir } from 'node:fs/promises';
import type { Plugin } from '@opencode/plugin';
import type { StandardSchemaV1 } from '@standard-schema/spec';
import {
  cancellationOf,
  CHECK_MODEL,
  chatHandler,
  checkModelName,
  type Configure,
  configured,
  messagesHandler,
  modelOf,
  providerOf,
  nextPromptId,
  settleSteering,
  type Steerable,
  toolCallEnded,
  toolCallStarted,
  type Transcript,
  transcribing,
} from '../common.ts';
import { type Answer, withOutput } from '../output.ts';
import type { z } from 'zod';
import { type Definition, object } from '../object.ts';
import { isTool, type Tool as AnyTool } from '../tools.ts';

import { type Fence, type Host, openOpenCode } from './db.ts';

/** An OpenCode model reference. */
export interface ModelRef {
  providerID: string;
  id: string;
}

/** `provider/model-id` as OpenCode names it. */
const modelRef = (name: string): ModelRef => {
  const [providerID, id] = providerOf(name);
  return { providerID, id };
};

/** An OpenCode tool of your own: `execute` gets the input and the tool call's id. */
export interface OpenCodeTool<Input = unknown> {
  name: string;
  description: string;
  input: StandardSchemaV1<unknown, Input>;
  execute(input: Input, callId: string): Promise<string>;
}

/** `t` as an OpenCode tool. */
const native = (t: AnyTool, ctx: restate.ObjectContext): OpenCodeTool => ({
  name: t.name,
  description: t.description,
  input: t.input,
  execute: (input, callId) => t.run(input as Record<string, unknown>, { ctx, callId }),
});

/** The session every prompt goes to. */
const SESSION = 'ses_root';

/** The key's database where OpenCode's tables are, apart from `ctx.db`. */
const DATABASE = 'restate-agents.opencode';

/** What `work` gets: `prompt(text)`, and the OpenCode `host` for anything else. */
export interface OpenCodeSession {
  host: Host;
  /** Answer `text`, once per prompt `id`, with the answer's text. By default, each prompt of an invocation has an id of its own, the same on every attempt. */
  prompt: (text: string, id?: string) => Promise<string>;
}

/** An agent's options: `chat`, `prompt` and `withOpenCode` take them. */
export interface Options<O extends z.ZodObject | undefined = z.ZodObject | undefined> {
  /** The model of the session, as `provider/model-id`: MODEL, or else openai/gpt-6-sol. */
  model?: string | ModelRef;
  /** The system prompt: who the agent is, and how it answers. */
  system?: string;
  /** The agent's tools, which replace OpenCode's own: `tool()`s, Restate's (`call`, `signal`...), or OpenCode tools of your own. */
  tools?: (AnyTool | OpenCodeTool)[];
  /**
   * A directory of the local machine, made if missing: OpenCode keeps its own tools (read,
   * write, edit, bash, glob, grep...) there, besides `tools`, and the
   * session works in it. Or a function of the invocation's context that
   * gives one: `(ctx) => \`/work/${ctx.key}\``, a directory per key; or makes
   * one, such as a git worktree (it runs at each attempt: make it idempotent).
   */
  workspace?: string | ((ctx: restate.ObjectContext) => string | Promise<string>);
  /** OpenCode plugins, such as the one of the model. */
  plugins?: Plugin.Plugin[];
  /**
   * The answer as data: a zod object, whose JSON the agent replies with
   * (checked), and `chat` and `prompt` answer with (see output.ts).
   */
  output?: O;
}

/** An OpenCode plugin that adds `system` to the session's system prompt. */
const systemPlugin = (system: string): Plugin.Plugin => ({
  id: 'restate-agents.system',
  async setup(ctx) {
    const hook = await ctx.session.hook('context', (request) => {
      request.system.push({ type: 'text', text: system });
    });
    return () => hook.dispose();
  },
});

/** An OpenCode plugin whose tools are `tools`, and only those. */
function toolsPlugin(tools: OpenCodeTool[], fail: (error: unknown) => Promise<never>, builtins = false): Plugin.Plugin {
  return {
    id: 'restate-agents.tools',
    async setup(ctx) {
      const transform = await ctx.tool.transform((editor) => {
        // Without a workspace, no filesystem, shell or web: the agent acts
        // through its tools.
        if (!builtins) for (const { id } of editor.list()) editor.remove(id);
        for (const t of tools) {
          editor.add({
            name: t.name,
            description: t.description,
            input: t.input,
            // Offered to the model as a tool, not in a code-mode script.
            options: { codemode: false },
            execute: async (input, context) => {
              try {
                return { content: await t.execute(input, context.id) };
              } catch (error) {
                return fail(error);
              }
            },
          });
        }
      });
      return () => transform.dispose();
    },
  };
}

/**
 * An OpenCode plugin that publishes each tool call as it starts and ends
 * (for the clients that follow the conversation), and gives the session the
 * messages steered to the turn (`steerable`) at its end: OpenCode's
 * steering, which its next step takes.
 */
const steeringPlugin = (
  ctx: restate.ObjectContext,
  steerable: Steerable,
  fail: (error: unknown) => Promise<never>,
): Plugin.Plugin => ({
  id: 'restate-agents.steering',
  async setup(plugin) {
    const before = await plugin.tool.hook('execute.before', async () => {
      await toolCallStarted(ctx).catch(fail);
    });
    const after = await plugin.tool.hook('execute.after', async () => {
      await toolCallEnded(ctx, steerable).catch(fail);
    });
    return async () => {
      await before.dispose();
      await after.dispose();
    };
  },
});

/** OpenCode's message id of prompt (or steered message) `id`: letters, digits, `_` and `-`. */
const messageIdOf = (id: string) => `msg_${id.replace(/[^\w-]/g, '_')}`;

/** Whether message `id` is one steered to a turn (see `steer` in object.ts), not a prompt. */
const steered = (id: string) => /_steer_\d+$/.test(id);

/** A part of an assistant message: text, or a tool call (`id`, `name`, and its `state`). */
interface Content {
  type: string;
  text?: string;
  id?: string;
  name?: string;
  state?: { status?: string; input?: unknown; content?: Content[]; error?: { message?: string } };
}

/** The text of `content`. */
const textOf = (content: Content[] = []) =>
  content.flatMap((c) => (c.type === 'text' && c.text ? [c.text] : [])).join('');

/** A message of the session's transcript, as far as a prompt's outcome and `messages` go. */
interface Message {
  id: string;
  type: string;
  outcome?: string;
  /** A user message's text. */
  text?: string;
  content?: Content[];
  error?: { message?: string };
}

/** The prompts waiting in the session's inbox, until their turn starts, oldest first. */
const inbox = (host: Host): Promise<{ id: string; payload: { text: string } }[]> =>
  host.call(host.client.session.inbox.list({ sessionID: SESSION as never }));

/** The session's messages, a page at a time, oldest or newest first. */
async function* messagePages(host: Host, order: 'asc' | 'desc'): AsyncGenerator<Message[]> {
  const sessionID = SESSION as never;
  for (let cursor: string | undefined; ;) {
    const page: { data: Message[]; cursor: { next?: string | null } } = await host.call(
      host.client.message.list(cursor === undefined ? { sessionID, order, limit: 50 } : { sessionID, cursor }),
    );
    yield page.data;
    cursor = page.cursor.next ?? undefined;
    if (cursor === undefined) return;
  }
}

/** The session's messages, oldest first. */
async function sessionMessages(host: Host): Promise<Message[]> {
  const all: Message[] = [];
  for await (const page of messagePages(host, 'asc')) all.push(...page);
  return all;
}

/** Where a prompt is: not in the session, its turn running, or ended with an answer or a failure. */
type Outcome =
  | { state: 'absent' | 'running' }
  | { state: 'answered'; text: string }
  | { state: 'failed'; reason: string };

/** Where prompt `messageId` is (see {@link Outcome}). */
async function outcome(host: Host, messageId: string): Promise<Outcome> {
  if ((await inbox(host)).some((item) => item.id === messageId)) return { state: 'running' };
  const after = await messagesAfter(host, messageId);
  return after ? turnOutcome(after) : { state: 'absent' };
}

/** The messages after prompt `messageId`, oldest first; undefined if the session does not have it. */
async function messagesAfter(host: Host, messageId: string): Promise<Message[] | undefined> {
  const after: Message[] = [];
  for await (const page of messagePages(host, 'desc')) {
    const at = page.findIndex((m) => m.id === messageId);
    after.unshift(...(at < 0 ? page : page.slice(0, at)).toReversed());
    if (at >= 0) return after;
  }
  return undefined;
}

/**
 * How the turn of the messages `after` a prompt ended: at an idle marker, or
 * where the next prompt, queued while it ran, starts (a message steered to it
 * is not one). Running, if neither came yet.
 */
function turnOutcome(after: Message[]): Outcome {
  let last: Message | undefined;
  for (const m of after) {
    if (m.type === 'idle') return ended(last, m.outcome);
    if (m.type === 'user' && last !== undefined && !steered(m.id)) return ended(last, 'succeeded');
    if (m.type === 'assistant') last = m;
  }
  return { state: 'running' };
}

/** The outcome of a turn that ended `how`, with `last`, its last assistant message: its text, or its error. */
function ended(last: Message | undefined, how: string | undefined): Outcome {
  const error = last?.error?.message;
  if (how === 'succeeded' && error === undefined) return { state: 'answered', text: textOf(last?.content) };
  return { state: 'failed', reason: error ?? how ?? 'failed' };
}

/** The id of the session's latest prompt, if any. */
async function lastPrompt(host: Host): Promise<string | undefined> {
  const queued = (await inbox(host)).findLast((item) => !steered(item.id));
  if (queued) return queued.id;
  for await (const page of messagePages(host, 'desc')) {
    const user = page.find((m) => m.type === 'user' && !steered(m.id));
    if (user) return user.id;
  }
  return undefined;
}

/** Whether a turn of the session runs. */
const active = async (host: Host) => {
  const turns: Record<string, unknown> = await host.call(host.client.session.active());
  return turns[SESSION] !== undefined;
};

/** Whether the session has message `messageId`: in its inbox, or in its transcript. */
const inSession = async (host: Host, messageId: string) => {
  const given = (m: { id: string }) => m.id === messageId;
  return (await inbox(host)).some(given) || (await sessionMessages(host)).some(given);
};

/**
 * Start the session's queued prompts, if no turn runs them. OpenCode resumes
 * the turns it claimed when it boots, but a crash between a prompt's queueing
 * and its turn's claim leaves it in the inbox with nothing to run it: sent
 * again, by its id, it starts.
 */
async function startInbox(host: Host) {
  if (await active(host)) return;
  const [first] = await inbox(host);
  if (first === undefined) return;
  await host.call(
    host.client.session.prompt({
      sessionID: SESSION as never,
      id: first.id as never,
      text: first.payload.text,
      delivery: 'queue',
      resume: true,
    }),
  );
}

/** The session, made with `model` (working in `workspace`, if given) unless it is there already. */
async function openSession(host: Host, model: ModelRef, workspace: string | undefined) {
  const { session } = host.client;
  const sessionID = SESSION as never;
  const exists = await host.call(session.get({ sessionID })).then(
    () => true,
    () => false,
  );
  if (exists) return;
  const location = workspace ? { location: { directory: workspace as never } } : {};
  await host.call(session.create({ id: sessionID, model: model as never, agent: 'build' as never, ...location }));
}

/**
 * Queue prompt `messageId`, unless the session has it. A prompt that a crash
 * interrupted finishes first: queued now, the new prompt would join its
 * resumed turn.
 */
async function submit(host: Host, messageId: string, text: string) {
  if ((await outcome(host, messageId)).state !== 'absent') return;
  const last = await lastPrompt(host);
  if (last !== undefined) await settled(host, last);
  await host.call(
    host.client.session.prompt({
      sessionID: SESSION as never,
      id: messageId as never,
      text,
      delivery: 'queue',
      resume: true,
    }),
  );
}

/** The outcome of prompt `messageId`, once its turn has ended. */
async function settled(host: Host, messageId: string): Promise<Outcome> {
  for (;;) {
    const now = await outcome(host, messageId);
    if (now.state !== 'running') return now;
    await startInbox(host);
    await host.call(host.client.session.wait({ sessionID: SESSION as never }));
    // A turn resumed at boot may not have started yet.
    if (!(await active(host))) await new Promise((resolve) => setTimeout(resolve, 20));
  }
}

/**
 * `failure`, once it comes. A cancellation first interrupts the session's
 * turn, which OpenCode records, so the next prompt does not resume it.
 */
async function interruptOn(host: Host, failure: Promise<never>): Promise<never> {
  try {
    return await failure;
  } catch (error) {
    if (error instanceof restate.CancelledError) {
      await host.call(host.client.session.interrupt({ sessionID: SESSION as never }));
    }
    throw error;
  }
}

/**
 * Run `work(opencode)` with OpenCode over the object's database: `model` is
 * the session's model, `plugins` OpenCode plugins (such as the model's), and
 * `tools` the agent's tools. `opencode` has `prompt(text)`, and the
 * OpenCode `host` for anything else.
 */
export const withOpenCode = async <T>(
  ctx: restate.ObjectContext,
  { plugins = [], model: named, system, tools = [], workspace: where }: Options,
  work: (opencode: OpenCodeSession) => Promise<T>,
): Promise<T> => {
  const model = typeof named === 'object' ? named : modelRef(modelOf(named));
  const workspace = typeof where === 'function' ? await where(ctx) : where;
  if (workspace) await mkdir(workspace, { recursive: true });
  const fence: Fence = { closed: false };
  const { promise: failure, reject: failed } = Promise.withResolvers<never>();
  failure.catch(() => {});
  // A tool's error ends the attempt: nothing more is stored, and the tool
  // call never returns, so OpenCode does not go on without it. A cancelled
  // invocation, whenever it comes, interrupts the session's turn instead
  // (interruptOn).
  let cancelled: restate.CancelledError | undefined;
  const fail = (error: unknown) => {
    if (error instanceof restate.CancelledError) cancelled = error;
    else fence.closed = true;
    failed(error);
    return new Promise<never>(() => {});
  };
  const turn = cancellationOf(ctx);
  const onCancel = () => void fail(turn!.reason);
  turn?.addEventListener('abort', onCancel, { once: true });
  // Messages steered to the turn: OpenCode's steering input (by its message
  // id, once), which the session takes at its next step.
  const steerable: Steerable = {
    steer: async ({ id, text }) => {
      const steer = { sessionID: SESSION as never, id: messageIdOf(id) as never, text, delivery: 'steer' as const };
      await host.call(host.client.session.prompt(steer));
    },
    has: (id) => inSession(host, messageIdOf(id)),
  };
  const host = await openOpenCode(databaseOf(ctx.db, DATABASE), fence, {
    plugins: [
      ...plugins,
      steeringPlugin(ctx, steerable, fail),
      toolsPlugin(
        tools.map((t) => (isTool(t) ? native(t, ctx) : t)),
        fail,
        workspace !== undefined,
      ),
      ...(system ? [systemPlugin(system)] : []),
    ],
    config: { default_agent: 'build' },
    local: workspace !== undefined,
  });

  const prompt = async (text: string, id = nextPromptId(ctx)) => {
    const messageId = messageIdOf(id);
    await openSession(host, model, workspace);
    await submit(host, messageId, text);
    const now = await settled(host, messageId);
    if (now.state === 'answered') {
      await settleSteering(ctx, steerable);
      return now.text;
    }
    if (now.state !== 'failed') throw new Error(`prompt ${id} is not in the session`);
    // A prompt the cancellation's interrupt ended fails with the cancellation.
    if (cancelled) throw cancelled;
    const of = `the model ${model.providerID}/${model.id}: ${CHECK_MODEL}`;
    throw new restate.TerminalError(`prompt ${id} unanswered: ${now.reason} (${of})`);
  };

  try {
    // The log reads the session through this host, while it is open.
    const read = (transcript: Transcript) => transcribe(host, transcript);
    return await transcribing(ctx, read, () => Promise.race([interruptOn(host, failure), work({ host, prompt })]));
  } finally {
    turn?.removeEventListener('abort', onCancel);
    await host.close();
  }
};

/** {@link prompt}, answering with the agent's text. */
const promptText = (ctx: restate.ObjectContext, text: string, options: Options): Promise<string> =>
  withOpenCode(ctx, options, (session) => session.prompt(text));

/**
 * Answer `text`, once: a retried attempt resumes the same prompt. Each prompt
 * of an invocation is a prompt of its own.
 * With `output`, it answers with the value the agent replies with (see
 * output.ts), not its text.
 */
export const prompt = <O extends z.ZodObject | undefined = undefined>(
  ctx: restate.ObjectContext,
  text: string,
  options: Options<O> = {},
): Promise<Answer<O>> => withOutput(text, options, (t, o) => promptText(ctx, t, o)) as Promise<Answer<O>>;

/**
 * A chat handler: each message an invocation, the session in the object's
 * database, with `options`, or the options a function of the invocation's
 * context gives (each user's workspace, say). It takes the message as plain
 * text (`curl -d 'Hi'`) or as a JSON string (`curl --json '"Hi"'`).
 */
export const chat = <O extends z.ZodObject | undefined = undefined>(options: Configure<Options<O>> = {}) => {
  if (typeof options === 'object' && typeof options.model !== 'object') checkModelName('opencode', options.model);
  return chatHandler(async (ctx, message) => prompt(ctx, message, await configured(options, ctx)));
};

/**
 * A handler of the conversation as clients read it: a page at a time, or
 * what is new or changed since a cursor, from the log each change writes
 * (log.ts). It answers while a message is answered.
 */
export const messages = () => messagesHandler();

/** The session of `host`, as `messages` gives it. */
const transcribe = async (host: Host, transcript: Transcript) => {
  for (const m of await sessionMessages(host)) {
    if (m.type === 'user') transcript.say('user', m.text);
    if (m.type !== 'assistant') continue;
    transcript.say('assistant', textOf(m.content));
    for (const c of m.content ?? []) {
      if (c.type !== 'tool' || !c.id || !c.state) continue;
      transcript.call(c.id, c.name ?? '', c.state.input);
      // A failed or interrupted call has its error, and perhaps no content.
      const output = c.state.content ? textOf(c.state.content) : c.state.error?.message;
      if (output !== undefined) transcript.result(c.id, output, c.state.status === 'error');
    }
  }
};

/** An agent: a virtual object `name`, one per key, whose `chat` handler is {@link chat}, and `messages` {@link messages}. */
export const agent = <P extends string, O extends z.ZodObject | undefined = undefined>({
  name,
  description,
  schema,
  background,
  ...options
}: Options<O> & Definition<P>) =>
  object({ name, description, schema, background, handlers: { chat: chat(options), messages: messages() } });
