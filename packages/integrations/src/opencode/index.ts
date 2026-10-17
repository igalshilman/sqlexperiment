// OpenCode 2.0 agents in Restate virtual objects: OpenCode for each
// invocation, with its database in the object's database.
//
//   const coder = opencode.agent({ name: 'coder', directory: (ctx) => `/work/${ctx.key}` });
//
// or a handler of an object of your own: `handlers: { chat: opencode.chat() }`
//
// or, for more than a prompt:
//
//   code: (ctx, task) => withOpenCode(ctx, { tools }, (session) => session.prompt(task)),
//
// The model is `model`, MODEL, or else anthropic/claude-sonnet-5-5,
// which OpenCode's own providers serve, with the provider's API key (such as
// ANTHROPIC_API_KEY).
//
// It runs `work` over `ctx.db`: OpenCode's tables are the object's
// database (db.ts), and each OpenCode transaction is an SQL
// transaction in it, stored in order with everything else the handler does.
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
import * as restate from '@restatedev/restate-sdk';
import { mkdir } from 'node:fs/promises';
import type { Plugin } from '@opencode/plugin';
import type { StandardSchemaV1 } from '@standard-schema/spec';
import { chatHandler, forCall, modelOf, type PerCall } from '../common.ts';
import { object } from '../object.ts';
import { isTool, type Tool as AnyTool } from '../tools.ts';

import { type Fence, type Host, openOpenCode } from './db.ts';

/** An OpenCode model reference. */
export interface ModelRef {
  providerID: string;
  id: string;
}

/** The default model, as `provider/model-id`. */
export const DEFAULT_MODEL = 'anthropic/claude-sonnet-5-5';

/** `provider/model-id` as OpenCode names it. */
const modelRef = (name: string): ModelRef => {
  const [providerID = '', ...id] = name.split('/');
  return { providerID, id: id.join('/') };
};

/** A tool: `execute` gets the input and the tool call's id. */
export interface Tool<Input = unknown> {
  name: string;
  description: string;
  input: StandardSchemaV1<unknown, Input>;
  execute(input: Input, callId: string): Promise<string>;
}

/** `t` as a tool of any input, for a list of tools of different inputs. */
const anyTool = <Input>(t: Tool<Input>) => t as Tool;

/** The session every prompt goes to. */
export const SESSION = 'ses_root';

/** What `work` gets: `prompt(text)`, and the OpenCode `host` for anything else. */
export interface OpenCodeSession {
  host: Host;
  /** Answer `text`, once per prompt `id` (by default the invocation's), with the answer's text. */
  prompt(text: string, id?: string): Promise<string>;
}

export interface OpenCodeOptions {
  /** OpenCode plugins, such as the one of the model. */
  plugins?: Plugin.Plugin[];
  /** The system prompt: who the agent is, and how it answers. */
  system?: string;
  /** The model of the session, as `provider/model-id`: MODEL, or else {@link DEFAULT_MODEL}. */
  model?: string | ModelRef;
  /** The agent's tools, which replace OpenCode's own. */
  tools?: Tool[];
  /**
   * A directory of the local machine, made if missing: OpenCode keeps its own tools (read,
   * write, edit, bash, glob, grep...) there, besides `tools`, and the
   * session works in it.
   */
  directory?: string;
}

/** An OpenCode plugin that adds `system` to the session's system prompt. */
const systemPlugin = (system: string): Plugin.Plugin => ({
  id: 'examples.system',
  async setup(ctx) {
    const hook = await ctx.session.hook('context', (request) => {
      request.system.push({ type: 'text', text: system });
    });
    return () => hook.dispose();
  },
});

/** An OpenCode plugin whose tools are `tools`, and only those. */
function toolsPlugin(tools: Tool[], fail: (error: unknown) => Promise<never>, builtins = false): Plugin.Plugin {
  return {
    id: 'examples.tools',
    async setup(ctx) {
      const transform = await ctx.tool.transform((editor) => {
        // Without a directory, no filesystem, shell or web: the agent acts
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

/** A message of the session's transcript, as far as a prompt's outcome goes. */
interface Message {
  id: string;
  type: string;
  outcome?: string;
  content?: { type: string; text?: string }[];
  error?: { message?: string };
}

/**
 * Where prompt `messageId` is: absent, unanswered yet, or answered with the
 * text of the last assistant message before the turn's idle marker.
 */
async function outcome(host: Host, messageId: string) {
  // A prompt waits in the inbox until its turn starts.
  const { session, message } = host.client;
  const inbox: { id: string }[] = await host.call(session.inbox.list({ sessionID: SESSION as never }));
  if (inbox.some((item) => item.id === messageId)) return { state: 'running' } as const;
  const after: Message[] = [];
  for (let cursor: string | undefined; ;) {
    const page: { data: Message[]; cursor: { next?: string | null } } = await host.call(
      message.list(
        cursor === undefined
          ? { sessionID: SESSION as never, order: 'desc', limit: 50 }
          : { sessionID: SESSION as never, cursor },
      ),
    );
    const messages = page.data;
    const at = messages.findIndex((m) => m.id === messageId);
    if (at >= 0) {
      after.unshift(...messages.slice(0, at).toReversed());
      break;
    }
    after.unshift(...messages.toReversed());
    cursor = page.cursor.next ?? undefined;
    if (cursor === undefined) return { state: 'absent' } as const;
  }
  // A turn ends at an idle marker, or where the next prompt, queued while it
  // ran, starts.
  let last: Message | undefined;
  for (const m of after) {
    if (m.type === 'idle' || (m.type === 'user' && last !== undefined)) {
      const text = (last?.content ?? []).flatMap((c) => (c.type === 'text' && c.text ? [c.text] : [])).join('');
      const error = last?.error?.message;
      const ended = m.type === 'idle' ? m.outcome : 'succeeded';
      return ended === 'succeeded' && error === undefined
        ? ({ state: 'answered', text } as const)
        : ({ state: 'failed', reason: error ?? ended ?? 'failed' } as const);
    }
    if (m.type === 'assistant') last = m;
  }
  return { state: 'running' } as const;
}

/** The id of the session's latest prompt, if any. */
async function lastPrompt(host: Host): Promise<string | undefined> {
  const { session, message } = host.client;
  const inbox: { id: string }[] = await host.call(session.inbox.list({ sessionID: SESSION as never }));
  if (inbox.length > 0) return inbox.at(-1)!.id;
  for (let cursor: string | undefined; ;) {
    const page: { data: Message[]; cursor: { next?: string | null } } = await host.call(
      message.list(
        cursor === undefined
          ? { sessionID: SESSION as never, order: 'desc', limit: 50 }
          : { sessionID: SESSION as never, cursor },
      ),
    );
    const user = page.data.find((m) => m.type === 'user');
    if (user) return user.id;
    cursor = page.cursor.next ?? undefined;
    if (cursor === undefined) return undefined;
  }
}

/**
 * Run `work(opencode)` with OpenCode over the object's database: `model` is
 * the session's model, `plugins` OpenCode plugins (such as the model's), and
 * `tools` the agent's tools. `opencode` has `prompt(text)`, and the
 * OpenCode `host` for anything else.
 */
export const withOpenCode = async <T>(
  ctx: restate.SqlObjectContext,
  { plugins = [], model: named, system, tools = [], directory }: OpenCodeOptions,
  work: (opencode: OpenCodeSession) => Promise<T>,
): Promise<T> => {
  const model = typeof named === 'object' ? named : modelRef(modelOf(named, DEFAULT_MODEL));
  if (directory) await mkdir(directory, { recursive: true });
  const db = ctx.db;
  const fence: Fence = { closed: false };
  let failed!: (error: unknown) => void;
  const failure = new Promise<never>((_, reject) => (failed = reject));
  failure.catch(() => {});
  // A tool's error ends the attempt: nothing more is stored, and the tool
  // call never returns, so OpenCode does not go on without it.
  const fail = (error: unknown) => {
    fence.closed = true;
    failed(error);
    return new Promise<never>(() => {});
  };
  const host = await openOpenCode(db, fence, {
    plugins: [...plugins, toolsPlugin(tools, fail, directory !== undefined), ...(system ? [systemPlugin(system)] : [])],
    config: { default_agent: 'build' },
    local: directory !== undefined,
  });
  try {
    return await Promise.race([
      failure,
      work({
        host,
        async prompt(text, id = ctx.request().id) {
          // OpenCode's ids are letters, digits, `_` and `-`.
          const messageId = `msg_${id.replace(/[^\w-]/g, '_')}`;
          const { session } = host.client;
          const sessionID = SESSION as never;
          try {
            await host.call(session.get({ sessionID }));
          } catch {
            await host.call(
              session.create({
                id: sessionID,
                model: model as never,
                agent: 'build' as never,
                ...(directory ? { location: { directory: directory as never } } : {}),
              }),
            );
          }
          // The outcome of prompt `of`, once its turn has ended.
          const settled = async (of: string) => {
            for (;;) {
              const now = await outcome(host, of);
              if (now.state !== 'running') return now;
              await host.call(session.wait({ sessionID }));
              // A turn resumed at boot may not have started yet.
              const active: Record<string, unknown> = await host.call(session.active());
              if (active[SESSION] === undefined) await new Promise((r) => setTimeout(r, 20));
            }
          };
          if ((await outcome(host, messageId)).state === 'absent') {
            // A prompt a crash interrupted finishes first: queued now, the
            // new prompt would join its resumed turn.
            const last = await lastPrompt(host);
            if (last !== undefined) await settled(last);
            await host.call(
              session.prompt({ sessionID, id: messageId as never, text, delivery: 'queue', resume: true }),
            );
          }
          const now = await settled(messageId);
          if (now.state === 'answered') return now.text;
          if (now.state === 'failed') throw new restate.TerminalError(`prompt ${id} unanswered: ${now.reason}`);
          throw new Error(`prompt ${id} is not in the session`);
        },
      }),
    ]);
  } finally {
    await host.close();
  }
};

/** Answer `text`, once per invocation: a retried attempt resumes the same prompt. */
export const prompt = (ctx: restate.SqlObjectContext, text: string, options: OpenCodeOptions = {}) =>
  withOpenCode(ctx, options, (session) => session.prompt(text));

export interface ChatOptions {
  /** The model, as `provider/model-id`: MODEL, or else {@link DEFAULT_MODEL}. */
  model?: string;
  /** The system prompt: who the agent is, and how it answers. */
  system?: string;
  /** The agent's tools, which replace OpenCode's own: `tool()`s, Restate's (`call`, `signal`...), or OpenCode tools of your own (`Tool`), or for an invocation. */
  tools?: (PerCall<Tool> | AnyTool)[];
  /** The directory to work in, with OpenCode's own tools (see {@link OpenCodeOptions.directory}). */
  directory?: PerCall<string | undefined>;
}

/** The answer to a message, each message an invocation, the session in the object's database. */
const respond =
  ({ model, system, tools = [], directory }: ChatOptions = {}) =>
  (ctx: restate.SqlObjectContext, message: string): Promise<string> =>
    prompt(ctx, message, {
      model,
      system,
      tools: tools.map((t) =>
        isTool(t)
          ? anyTool({
              name: t.name,
              description: t.description,
              input: t.input,
              execute: (input, callId) => t.run(input, { ctx, callId }),
            })
          : forCall(t, ctx),
      ),
      directory: directory === undefined ? undefined : forCall(directory, ctx),
    });

/**
 * A chat handler: each message an invocation, the session in the object's database.
 * It takes the message as plain text (`curl -d 'Hi'`) or as a JSON string
 * (`curl --json '"Hi"'`).
 */
export const chat = (options: ChatOptions = {}) => chatHandler(respond(options));

/** An agent: a virtual object `name`, one per key, whose `chat` handler is {@link chat}. */
export const agent = <P extends string>({
  name,
  description,
  ...options
}: ChatOptions & { name: P; description?: string }) => object({ name, description, handlers: { chat: chat(options) } });
