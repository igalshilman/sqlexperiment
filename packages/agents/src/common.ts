// What the frameworks share: their options (`Configure`, the model), the
// ids of an invocation's prompts, the `chat` handler with its turn (status,
// cancellation, steering, publishing for the clients that follow it), and
// the `messages` handler with its `Transcript`.
import * as restate from 'restate-objects';
import { settle } from './calls.ts';
import * as log from './log.ts';
import type { Said } from './log.ts';
import { agentOf, type Steer, signals, signalsOf } from './signals.ts';

/**
 * A value, or a function of the invocation's context that gives it: the
 * same for every message, or each user's (from the object's database, say).
 *
 *   pi.chat({ model: 'openai/gpt-6-sol' })
 *   pi.chat(async (ctx) => ({ model: await userModel(ctx) }))
 */
export type Configure<T> = T | ((ctx: restate.ObjectContext) => T | Promise<T>);

/** `value` for the invocation of `ctx`. */
export const configured = async <T>(value: Configure<T>, ctx: restate.ObjectContext): Promise<T> =>
  typeof value === 'function' ? (value as (ctx: restate.ObjectContext) => T | Promise<T>)(ctx) : value;

/** The prompts each invocation made so far, by its context. */
const prompts = new WeakMap<object, number>();

/**
 * The id of the next prompt of the invocation of `ctx`: the invocation's id,
 * then `<id>:2`, `<id>:3`... A retried attempt makes its prompts in the same
 * order, so each gets the same id again, and its answer.
 */
export const nextPromptId = (ctx: restate.ObjectContext) => {
  const n = (prompts.get(ctx) ?? 0) + 1;
  prompts.set(ctx, n);
  return n === 1 ? ctx.request().id : `${ctx.request().id}:${n}`;
};

/** The default model of pi, OpenCode and LangGraph, with OPENAI_API_KEY. */
export const DEFAULT_MODEL = 'openai/gpt-6-sol';

/** The default model of Claude agents, with ANTHROPIC_API_KEY. */
export const CLAUDE_MODEL = 'anthropic/claude-sonnet-5-5';

/**
 * The model, as `provider/model-id` (such as `openai/gpt-6-sol`):
 * `model`, or else MODEL, or else the framework's default.
 */
export const modelOf = (model: string | undefined, fallback = DEFAULT_MODEL) => model || process.env.MODEL || fallback;

/** `provider/model-id` as its provider and model id, split at the first `/` (`openrouter/meta-llama/llama-4`). */
export const providerOf = (model: string): [provider: string, id: string] => {
  const slash = model.indexOf('/');
  return slash < 0 ? ['', model] : [model.slice(0, slash), model.slice(slash + 1)];
};

/**
 * The error code of a message the model's provider refused (a missing or
 * wrong API key, an unknown model): 424, Failed Dependency, as its HTTP
 * status.
 */
export const REFUSED = 424;

/** What to check when a model's provider refuses a request. */
export const CHECK_MODEL = "check the agent's model (its `model` option, or MODEL) and its provider's API key";

/** An agent's model that it cannot take: not `provider/model-id`, or another provider's. */
export class ModelError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'ModelError';
  }
}

/** `model`, the explicit `model` option of `framework`'s agent, if it is not `provider/model-id`. */
export const checkModelName = (framework: string, model: string | undefined) => {
  if (model !== undefined && !/^[^/\s]+\/\S+$/.test(model)) {
    throw new ModelError(`${framework}: the model "${model}" is not provider/model-id, such as openai/gpt-6-sol`);
  }
};

/**
 * The model id of `model`, for a framework with only `provider`'s models:
 * `provider/model-id`, or the id alone.
 */
export const modelIdOf = (model: string, provider: string) => {
  const [named, id] = model.includes('/') ? providerOf(model) : [provider, model];
  if (named !== provider || !id) {
    const from = process.env.MODEL === model ? ' (from MODEL)' : '';
    throw new ModelError(`the model "${model}"${from} is not ${provider}'s: this agent takes ${provider}/<model-id>`);
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

/**
 * What a prompt answers that the model's provider refused for good (see
 * {@link refusal}): the prompt is over, and the model sees it in the
 * conversation that goes on, rather than answer it with the next message.
 */
export const UNANSWERED = "Unanswered: the model's provider refused the request.";

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

/** The agents that run in the background (`background: true`): no one follows them as they work. */
const background = new Set<string>();

/** Agent `name` runs in the background: see `background` in object.ts. */
export const inBackground = (name: string) => void background.add(name);

/** Whether the agent of `ctx` runs in the background. */
const unfollowed = (ctx: restate.ObjectContext) => background.has(ctx.request().target.service);

/** The `signals` object of the agent of `ctx`. */
const signalsOfAgent = (ctx: restate.ObjectContext) => ctx.objectClient(signals, signalsOf(ctx));

/**
 * The conversation of `ctx` changed (see `watch` in object.ts): its log takes
 * the change (while a framework answers: {@link transcribing}), its revision
 * increases, and the clients that follow it read it again. It is a call to
 * Restate, so it also sends what the handler wrote before, for them to read.
 */
const publish = async (ctx: restate.ObjectContext) => {
  const read = reading.get(ctx);
  if (read) await logged(ctx, read);
  await signalsOfAgent(ctx).publish();
};

/** A step of a turn of `ctx`: {@link publish} it, unless the agent runs in the background, which publishes the turn's end only. */
const stepped = async (ctx: restate.ObjectContext) => {
  if (!unfollowed(ctx)) await publish(ctx);
};

/** A tool call of the turn of `ctx` starts: {@link publish} it (its call is stored). */
export const toolCallStarted = (ctx: restate.ObjectContext) => stepped(ctx);

/**
 * A tool call of the turn of `ctx` (a `chat` handler's) ended: {@link publish}
 * it, and give `conversation` the messages steered to the turn since, which it
 * does not have yet: its next step takes them. Without a conversation (a
 * framework that takes no steering), they wait for the turn's end.
 */
export async function toolCallEnded(ctx: restate.ObjectContext, conversation?: Steerable) {
  // In the background, no one steers: a message steered anyway is answered after the turn.
  if (unfollowed(ctx)) return;
  const messages = await signalsOfAgent(ctx).progress(ctx.request().id);
  const taken = steerings.get(ctx);
  if (!taken || !conversation) return;
  for (const message of messages.filter((m) => !taken.has(m.id))) {
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
export async function settleSteering(ctx: restate.ObjectContext, conversation: Pick<Steerable, 'has'>) {
  const taken = steerings.get(ctx);
  if (!taken) return;
  for (const message of await signalsOfAgent(ctx).steering(ctx.request().id)) {
    if (!taken.has(message.id) && (await conversation.has(message.id))) taken.add(message.id);
  }
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
 * tool call ({@link toolCallEnded}); when it ends, those it did not take are
 * answered as messages of their own, unless it was cancelled. It publishes
 * when it starts and ends ({@link publish}), for the clients that follow the
 * conversation.
 */
export const chatHandler = <A = string>(answer: (ctx: restate.ObjectContext, message: string) => Promise<A>) =>
  restate.handlers.object.exclusive(
    { input: messageSerde, accept: '*/*', explicitCancellation: true },
    async (ctx: restate.ObjectContext, text: string) => {
      const invocationId = ctx.request().id;
      if ((await ctx.get<Turn>(TURN))?.invocationId !== invocationId) {
        ctx.set(TURN, { invocationId, since: new Date().toISOString() } satisfies Turn);
      }
      const taken = new Set<string>();
      steerings.set(ctx, taken);
      await stepped(ctx);
      const end = async (cancelled = false) => {
        ctx.clear(TURN);
        ctx
          .objectSendClient(signals, signalsOf(ctx))
          .close({ agent: agentOf(ctx), turn: invocationId, taken: [...taken], cancelled });
        await publish(ctx);
      };
      const turn = new AbortController();
      turns.set(ctx, turn.signal);
      const internal = restate.internal.asInternal(ctx as unknown as restate.Context);
      void (async () => {
        await (internal.cancellation() as unknown as Promise<void>);
        turn.abort(new restate.CancelledError());
        await (internal.cancelPreviousCalls() as unknown as Promise<unknown>);
      })().catch(() => {});
      let answered: A;
      try {
        answered = await answer(ctx, text);
        // Cancelled once answered: the answer stands.
      } catch (error) {
        if (error instanceof restate.TerminalError) {
          await end(error instanceof restate.CancelledError);
          throw error;
        }
        // Any other error fails the attempt, and Restate retries the turn.
        const terminal = refusal(error);
        if (!terminal) throw error;
        await end();
        throw terminal;
      }
      await end();
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
  if (error instanceof ModelError) return true;
  if (error instanceof ToolError) return false;
  if (error instanceof Error && FOR_GOOD.has(error.constructor.name)) return true;
  return /API key not found|no API key|Could not resolve authentication method|Provider is not configured|"code":"(AUTH|MISSING_CREDENTIAL)"/i.test(
    String(error),
  );
}

/**
 * `error` as a terminal error ({@link REFUSED}), if a retry does not change
 * it: a model the agent cannot take, or its provider's refusal.
 */
export function refusal(error: unknown): restate.TerminalError | undefined {
  if (error instanceof restate.TerminalError || !refused(error)) return undefined;
  const message =
    error instanceof ModelError
      ? error.message
      : `the model's provider refused the request: ${CHECK_MODEL}. ${String(error)}`;
  return new restate.TerminalError(message, { errorCode: REFUSED });
}

/** A message of a conversation, as an agent's `messages` handler returns it. */
export interface ChatMessage {
  /** Its id, from 0, in the order of the conversation; the same however it changes. `before` takes it. */
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

/**
 * Which messages to read: the latest `limit`, or those before a message, for
 * a page of the conversation; or those new or changed since a `cursor`, to
 * follow it.
 */
export interface MessagesRequest {
  /** At most this many messages (default 50). */
  limit?: number;
  /** The messages before this one (its `id`): older ones, such as for scrolling back. */
  before?: number;
  /** The messages new or changed since this cursor (a page's `cursor`), by when they changed. */
  cursor?: number;
}

/**
 * A page of messages: oldest first; or, with a `cursor`, those new or
 * changed since it, by when they changed. Keep them by `id`: a message comes
 * again once it changed (its tool call's output came, say).
 */
export interface Messages {
  messages: ChatMessage[];
  /** Whether there are more: older ones, or with a `cursor`, newer ones. */
  more: boolean;
  /** The cursor to read on from: what changes after this page. */
  cursor: number;
  /**
   * The id of the conversation's first message, once it was cleared
   * (`clear`): a client drops the messages it kept from before.
   */
  first?: number;
}

/**
 * A conversation as `messages` gives it, built from a framework's transcript:
 * each message is started by `say`, a tool call goes to the assistant's
 * message, and its result to its call. Messages without parts are left out.
 */
export class Transcript {
  #messages: Said[] = [];
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

  /** Whether tool call `id` has its output. */
  hasResult(id: string) {
    return this.#calls.get(id)?.output !== undefined;
  }

  /** Its messages, in order: those with parts. */
  messages(): Said[] {
    return this.#messages.filter((m) => m.parts.length > 0);
  }
}

/** What a framework's transcript is read into, while it answers (see {@link transcribing}). */
export type Read = (transcript: Transcript) => Promise<void>;

/** The transcript of the turn of each context, while it answers. */
const reading = new WeakMap<object, Read>();

/** The log writes of each context, one after another: tool calls in parallel publish at once. */
const writing = new WeakMap<object, Promise<void>>();

/** The log of the conversation of `ctx` takes its transcript as `read` gives it. */
function logged(ctx: restate.ObjectContext, read: Read): Promise<void> {
  const write = async () => {
    const transcript = new Transcript();
    await read(transcript);
    await log.sync(ctx.db, transcript.messages(), () => ctx.get<log.Start>(log.START));
    // The calls whose results the framework stored are made: see calls.ts.
    await settle(ctx, (id) => transcript.hasResult(id));
  };
  const done = (writing.get(ctx) ?? Promise.resolve()).then(write, write);
  writing.set(ctx, done);
  return done;
}

/**
 * Run `work`, a framework's turn (or a compaction, a reset), with `read`
 * giving the framework's transcript as it stands, from the framework as
 * `work` holds it open: each publish meanwhile (a tool call's start and end)
 * takes it into the conversation's log, and so does `work`'s end, before the
 * framework closes. `messages` and `sync` read the log.
 */
export async function transcribing<T>(ctx: restate.ObjectContext, read: Read, work: () => Promise<T>): Promise<T> {
  const outer = reading.get(ctx);
  reading.set(ctx, read);
  let failed = false;
  try {
    return await work();
  } catch (error) {
    failed = !(error instanceof restate.TerminalError);
    throw error;
  } finally {
    try {
      // A failed attempt stores nothing more: its retry runs again.
      if (!failed) await logged(ctx, read);
    } finally {
      if (outer) reading.set(ctx, outer);
      else reading.delete(ctx);
    }
  }
}

/**
 * The handler of an agent's conversation as clients read it, from its log:
 * a page of it (`{limit, before}`), or what is new or changed since a cursor
 * (`{cursor}`). It is shared: it answers while a message is answered, with
 * what was stored so far, and stores nothing.
 */
export const messagesHandler = () =>
  restate.handlers.object.shared(
    async (ctx: restate.ObjectSharedContext, request?: MessagesRequest): Promise<Messages> =>
      log.page(ctx.db, request ?? {}, await ctx.get<log.Start>(log.START)),
  );
