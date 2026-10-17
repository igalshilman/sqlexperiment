// What the integrations share.
import * as restate from '@restatedev/restate-sdk';
import { type Steer, signals, signalsOf } from './signals.ts';

/**
 * A value, or a function of the invocation's context that gives it: the
 * same for every message, or each user's (from the object's database, say).
 *
 *   pi.chat({ model: 'anthropic/claude-sonnet-5-5' })
 *   pi.chat(async (ctx) => ({ model: await modelOf(ctx) }))
 */
export type Configure<T> = T | ((ctx: restate.SqlObjectContext) => T | Promise<T>);

/** `value` for the invocation of `ctx`. */
export const configured = async <T>(value: Configure<T>, ctx: restate.SqlObjectContext): Promise<T> =>
  typeof value === 'function' ? (value as (ctx: restate.SqlObjectContext) => T | Promise<T>)(ctx) : value;

/** The prompts each invocation made so far, by its context. */
const prompts = new WeakMap<object, number>();

/**
 * The id of the next prompt of the invocation of `ctx`: the invocation's id,
 * then `<id>:2`, `<id>:3`... A retried attempt makes its prompts in the same
 * order, so each gets the same id again, and its answer.
 */
export const nextPromptId = (ctx: restate.SqlObjectContext) => {
  const n = (prompts.get(ctx) ?? 0) + 1;
  prompts.set(ctx, n);
  return n === 1 ? ctx.request().id : `${ctx.request().id}:${n}`;
};

/**
 * The model, as `provider/model-id` (such as `anthropic/claude-sonnet-5-5`):
 * `model`, or else MODEL, or else the framework's default.
 */
export const modelOf = (model: string | undefined, fallback: string) => model || process.env.MODEL || fallback;

/**
 * The error code of a message the model's provider refused (a missing or
 * wrong API key, an unknown model): 424, Failed Dependency, as its HTTP
 * status.
 */
export const REFUSED = 424;

/** What to check when a model's provider refuses a request. */
export const CHECK_MODEL = "check the agent's model (its `model` option, or MODEL) and its provider's API key";

/** `model`, the explicit `model` option of `framework`'s agent, if it is not `provider/model-id`. */
export const checkModelName = (framework: string, model: string | undefined) => {
  if (model !== undefined && !/^[^/\s]+\/\S+$/.test(model)) {
    throw new Error(`${framework}: the model "${model}" is not provider/model-id, such as anthropic/claude-sonnet-5-5`);
  }
};

/**
 * The model id of `model`, for a framework with only `provider`'s models:
 * `provider/model-id`, or the id alone.
 */
export const modelIdOf = (model: string, provider: string) => {
  const [named, id] = model.includes('/') ? model.split('/', 2) : [provider, model];
  if (named !== provider || !id) {
    const from = process.env.MODEL === model ? ' (from MODEL)' : '';
    throw new Error(`the model "${model}"${from} is not ${provider}'s: this agent takes ${provider}/<model-id>`);
  }
  return id;
};

/** A chat message: the request's body as text, or the string it is as JSON (`"Hi"`). */
export const messageSerde: restate.Serde<string> = {
  contentType: 'text/plain',
  serialize: (text) => new TextEncoder().encode(text),
  deserialize: (bytes) => {
    const text = new TextDecoder().decode(bytes);
    try {
      const value: unknown = JSON.parse(text);
      return typeof value === 'string' ? value : text;
    } catch {
      return text;
    }
  },
};

/**
 * What a cancelled turn answers, and each tool call it cut off: the model
 * sees it in the conversation that goes on.
 */
export const CANCELLED = 'Cancelled: the turn was stopped before it finished.';

/** The state key of the {@link Turn} being answered. */
export const TURN = 'restate-agents.turn';

/** The message a `chat` handler is answering: its invocation, and when it started. */
export interface Turn {
  /** The invocation's id: `/restate/invocation/<id>/attach` waits for its answer. */
  invocationId: string;
  /** When it started, as an ISO date. */
  since: string;
}

/** The cancellation of each turn, by its context: aborted when Restate cancels its invocation. */
const turns = new WeakMap<object, AbortSignal>();

/**
 * The signal of the turn of `ctx` (of a `chat` handler, `chatHandler`):
 * aborted, with a `restate.CancelledError`, once Restate cancels the
 * invocation. An agent's `prompt` stops its framework's turn then.
 */
export const cancellationOf = (ctx: object): AbortSignal | undefined => turns.get(ctx);

/** The ids of the messages steered to each turn that its conversation has, by its context. */
const steerings = new WeakMap<object, Set<string>>();

/** A framework's conversation, while it answers a message: what steering needs of it. */
export interface Steerable {
  /** Give it `message`, for its next step: once, by the message's id, however often it is given. */
  steer(message: Steer): Promise<void>;
  /** Whether it has message `id`: given in this attempt, or an earlier one. */
  has(id: string): Promise<boolean>;
}

/** The messages steered to the turn of `ctx` (see `steer` in object.ts), and the ids of those taken. */
async function steeringOf(ctx: restate.SqlObjectContext) {
  const taken = steerings.get(ctx);
  if (!taken) return { taken: new Set<string>(), messages: [] };
  const { service } = ctx.request().target;
  const messages = await ctx.objectClient(signals, signalsOf(service, ctx.key)).steering(ctx.request().id);
  return { taken, messages: messages.filter((m) => !taken.has(m.id)) };
}

/**
 * Give `conversation` the messages steered to the turn of `ctx` (a `chat`
 * handler's) since, which it does not have yet. A framework calls it at the
 * end of each tool call, while the turn goes on: its next step takes them.
 */
export async function takeSteering(ctx: restate.SqlObjectContext, conversation: Steerable) {
  const { taken, messages } = await steeringOf(ctx);
  for (const message of messages) {
    if (!(await conversation.has(message.id))) await conversation.steer(message);
    taken.add(message.id);
  }
}

/**
 * Count the messages steered to the turn of `ctx` that `conversation` has,
 * such as those an earlier attempt gave it. A framework calls it once a
 * prompt is answered; the turn's other messages are answered as messages of
 * their own, when it ends.
 */
export async function settleSteering(ctx: restate.SqlObjectContext, conversation: Pick<Steerable, 'has'>) {
  const { taken, messages } = await steeringOf(ctx);
  for (const message of messages) if (await conversation.has(message.id)) taken.add(message.id);
}

/** A promise that rejects with `signal`'s reason once it aborts. */
export const aborted = (signal: AbortSignal) =>
  new Promise<never>((_, reject) => {
    if (signal.aborted) reject(signal.reason);
    else signal.addEventListener('abort', () => reject(signal.reason), { once: true });
  });

/**
 * `answer` as a chat handler, which takes its message as plain text or as a
 * JSON string, so that both of these work:
 *
 *   curl localhost:8080/assistant/alice/chat -d 'Hi'
 *   curl localhost:8080/assistant/alice/chat --json '"Hi"'
 *
 * While it answers, the object's `status` says so (its {@link Turn}). Its
 * invocation's cancellation (Restate's, explicit: `ctx.cancellation()`)
 * aborts {@link cancellationOf} its context, whenever it comes, and cancels
 * the calls its tools made (`cancelPreviousCalls()`): the agent's `prompt`
 * ends the turn, and the handler fails with a `restate.CancelledError`.
 *
 * Messages steered to it (`steer`) go to the agent's conversation at its next
 * tool call ({@link takeSteering}); when it ends, those it did not take are
 * answered as messages of their own, unless it was cancelled.
 */
export const chatHandler = (answer: (ctx: restate.SqlObjectContext, message: string) => Promise<string>) =>
  restate.handlers.object.exclusive(
    { input: messageSerde, accept: '*/*', journalReplay: false, explicitCancellation: true },
    async (ctx: restate.SqlObjectContext, text: string) => {
      const invocationId = ctx.request().id;
      if ((await ctx.get<Turn>(TURN))?.invocationId !== invocationId) {
        ctx.set(TURN, { invocationId, since: new Date().toISOString() } satisfies Turn);
      }
      const taken = new Set<string>();
      steerings.set(ctx, taken);
      const agent = { service: ctx.request().target.service, key: ctx.key };
      const end = (cancelled = false) => {
        ctx.clear(TURN);
        ctx
          .objectSendClient(signals, signalsOf(agent.service, agent.key))
          .close({ agent, turn: invocationId, taken: [...taken], cancelled });
      };
      const turn = new AbortController();
      turns.set(ctx, turn.signal);
      const internal = restate.internal.asInternal(ctx as unknown as restate.Context);
      void (async () => {
        await (internal.cancellation() as unknown as Promise<void>);
        turn.abort(new restate.CancelledError());
        await (internal.cancelPreviousCalls() as unknown as Promise<unknown>);
      })().catch(() => {});
      let answered: string;
      try {
        answered = await answer(ctx, text);
        // Cancelled once answered: the answer stands.
      } catch (error) {
        if (error instanceof restate.TerminalError) {
          end(error instanceof restate.CancelledError);
          throw error;
        }
        // Any other error fails the attempt, and Restate retries the turn.
        if (!refused(error)) throw error;
        end();
        throw new restate.TerminalError(`the model's provider refused the request: ${CHECK_MODEL}. ${String(error)}`, {
          errorCode: REFUSED,
        });
      }
      end();
      return answered;
    },
  );

/** A tool's error, which fails the attempt (Restate retries it): the error `cause` of tool `tool`. */
export class ToolError extends Error {
  readonly tool: string;
  constructor(tool: string, cause: unknown) {
    super(`the tool ${tool} failed: ${cause instanceof Error ? cause.message : String(cause)}`, { cause });
    this.name = 'ToolError';
    this.tool = tool;
  }
}

/** The Anthropic SDK's errors of a request that fails again however often it is made. */
const FOR_GOOD = new Set(['AuthenticationError', 'PermissionDeniedError', 'NotFoundError', 'BadRequestError']);

/**
 * Whether `error` is a model provider's refusal that a retry does not change:
 * a missing or wrong API key, an unknown model, a bad request. Other errors
 * (a tool's, an overloaded provider's) fail the attempt, and Restate retries.
 */
function refused(error: unknown) {
  if (error instanceof ToolError) return false;
  if (error instanceof Error && FOR_GOOD.has(error.constructor.name)) return true;
  return /API key not found|no API key|Could not resolve authentication method|"code":"AUTH"/i.test(String(error));
}

/** A message of a conversation, as an agent's `messages` handler returns it. */
export interface ChatMessage {
  /** Its position in the conversation, from 0; `before` and `after` take it. */
  id: number;
  role: 'user' | 'assistant';
  parts: Part[];
}

/**
 * What a message says: text, or a tool call with its output once it ran,
 * and `error: true` if the output is the tool's error (`Error: <message>` of
 * a `restate.TerminalError`, say).
 */
export type Part =
  | { type: 'text'; text: string }
  | { type: 'tool'; id: string; name: string; input: unknown; output?: string; error?: true };

/** Which messages to read: the latest `limit`, or those before or after a message. */
export interface MessagesRequest {
  /** At most this many messages (default 50). */
  limit?: number;
  /** The messages before this one (its `id`): older ones, such as for scrolling back. */
  before?: number;
  /** The messages after this one (its `id`): newer ones, such as for polling. */
  after?: number;
}

/** A page of messages, oldest first. */
export interface Messages {
  messages: ChatMessage[];
  /** Whether there are more: older ones, or with `after`, newer ones. */
  more: boolean;
}

/**
 * A conversation as `messages` gives it, built from a framework's transcript:
 * each message is started by `say`, a tool call goes to the assistant's
 * message, and its result to its call. Messages without parts are left out.
 */
export class Transcript {
  #messages: Omit<ChatMessage, 'id'>[] = [];
  #calls = new Map<string, Extract<Part, { type: 'tool' }>>();

  /** A new message of `role`, with `text` if there is any. */
  say(role: ChatMessage['role'], text = '') {
    this.#messages.push({ role, parts: text.trim() ? [{ type: 'text', text }] : [] });
  }

  /** Tool call `id` of the assistant's message. */
  call(id: string, name: string, input: unknown) {
    if (this.#messages.at(-1)?.role !== 'assistant') this.say('assistant');
    const part = { type: 'tool' as const, id, name, input };
    this.#messages.at(-1)!.parts.push(part);
    this.#calls.set(id, part);
  }

  /** The output of tool call `id`; an error's if the framework says so, or it is a tool's `Error: <message>`. */
  result(id: string, output: string, error = false) {
    const call = this.#calls.get(id);
    if (!call) return;
    call.output = output;
    if (error || output.startsWith('Error: ')) call.error = true;
  }

  /** The page of `request`. */
  page({ limit = 50, before, after }: MessagesRequest = {}): Messages {
    const all = this.#messages.filter((m) => m.parts.length > 0).map((m, id) => ({ id, role: m.role, parts: m.parts }));
    if (after !== undefined) {
      const newer = all.slice(after + 1);
      return { messages: newer.slice(0, limit), more: newer.length > limit };
    }
    const end = before === undefined ? all.length : Math.max(0, Math.min(before, all.length));
    const start = Math.max(0, end - limit);
    return { messages: all.slice(start, end), more: start > 0 };
  }
}

/**
 * `read` as the handler of an agent's conversation, a page at a time: what
 * was said, in order, with the tool calls. It is shared: it answers while a
 * message is answered, from the database as stored (the tool calls made so
 * far, say), and stores nothing.
 */
export const messagesHandler = (read: (ctx: restate.SqlObjectSharedContext, transcript: Transcript) => Promise<void>) =>
  restate.handlers.object.shared(
    { journalReplay: false },
    async (ctx: restate.SqlObjectSharedContext, request?: MessagesRequest) => {
      const transcript = new Transcript();
      await read(ctx, transcript);
      return transcript.page(request ?? {});
    },
  );
