// What the frameworks share: their options (`Configure`, the model), the
// ids of an invocation's prompts, the `chat` handler with its turn (status,
// cancellation, steering, publishing for the clients that follow it), and
// the `messages` handler with its `Transcript`.
import * as restate from 'restate-session-objects';
import { cancelCalls, cancelled } from 'restate-session-objects/internal';
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
export type Configure<T> = T | ((ctx: restate.SessionContext) => T | Promise<T>);

/** `value` for the invocation of `ctx`. */
export const configured = async <T>(value: Configure<T>, ctx: restate.SessionContext): Promise<T> =>
  typeof value === 'function' ? (value as (ctx: restate.SessionContext) => T | Promise<T>)(ctx) : value;

/** The prompts each invocation made so far, by its context. */
const prompts = new WeakMap<object, number>();

/**
 * The id of the next prompt of the invocation of `ctx`: the invocation's id,
 * then `<id>:2`, `<id>:3`... A retried attempt makes its prompts in the same
 * order, so each gets the same id again, and its answer.
 */
export const nextPromptId = (ctx: restate.SessionContext) => {
  const n = (prompts.get(ctx) ?? 0) + 1;
  prompts.set(ctx, n);
  return n === 1 ? ctx.request.id : `${ctx.request.id}:${n}`;
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

/**
 * An agent's model that it cannot take: not `provider/model-id`, another
 * provider's, or one its framework does not know. Terminal ({@link REFUSED}),
 * wherever it is thrown: a retry takes the same model.
 */
export class ModelError extends restate.TerminalError {
  constructor(message: string) {
    super(message, { errorCode: REFUSED });
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

/** What each agent clears beside its database, by its object's name: see {@link onClear}. */
const clearing = new Map<string, (ctx: restate.SessionContext) => Promise<void>>();

/** What a framework's `messages` view clears beside the agent's database: see {@link alsoClearing}. */
const viewsClearing = new WeakMap<object, (ctx: restate.SessionContext) => Promise<void>>();

/**
 * The `messages` view `view` of a framework that keeps a conversation apart
 * (OpenCode, in a database of its own): `clear` empties it too, as an agent
 * with this view is cleared.
 */
export const alsoClearing = <V extends object>(view: V, clear: (ctx: restate.SessionContext) => Promise<void>) => {
  viewsClearing.set(view, clear);
  return view;
};

/** When the agent `name`, whose `messages` view is `view`, is cleared, clear what that view keeps apart too. */
export const onClear = (name: string, view: object | undefined) => {
  const clear = view === undefined ? undefined : viewsClearing.get(view);
  if (clear === undefined) clearing.delete(name);
  else clearing.set(name, clear);
};

/** Clear what the agent of `ctx` keeps beside its database ({@link onClear}). */
export const cleared = async (ctx: restate.SessionContext) => clearing.get(ctx.request.target.service)?.(ctx);

/** Whether the agent of `ctx` runs in the background. */
const unfollowed = (ctx: restate.SessionContext) => background.has(ctx.request.target.service);

/** The `signals` object of the agent of `ctx`. */
const signalsOfAgent = (ctx: restate.SessionContext) => ctx.rpc(signals, signalsOf(ctx));

/**
 * The conversation of `ctx` changed (see `watch` in object.ts): its log takes
 * the change (while a framework answers: {@link transcribing}), its revision
 * increases, and the clients that follow it read it again. It is a call to
 * Restate, so it also sends what the handler wrote before, for them to read.
 */
const publish = async (ctx: restate.SessionContext) => {
  const read = reading.get(ctx);
  if (read) await logged(ctx, read);
  await signalsOfAgent(ctx).publish();
};

/** A step of a turn of `ctx`: {@link publish} it, unless the agent runs in the background, which publishes the turn's end only. */
const stepped = async (ctx: restate.SessionContext) => {
  if (!unfollowed(ctx)) await publish(ctx);
};

/**
 * A tool call of the turn of `ctx` starts: {@link publish} it, which forgets
 * the calls whose results are stored (calls.ts) before the tool makes its
 * own. In the background, which publishes the turn's end only, it forgets
 * them alone.
 */
export async function toolCallStarted(ctx: restate.SessionContext) {
  if (!unfollowed(ctx)) return publish(ctx);
  const read = reading.get(ctx);
  if (read) {
    const transcript = await read();
    await settle(ctx, (id) => transcript.answered(id));
  }
}

/**
 * A tool call of the turn of `ctx` (a `chat` handler's) ended: {@link publish}
 * it, and give `conversation` the messages steered to the turn since, which it
 * does not have yet: its next step takes them. Without a conversation (a
 * framework that takes no steering), they wait for the turn's end.
 */
export async function toolCallEnded(ctx: restate.SessionContext, conversation?: Steerable) {
  // In the background, no one steers: a message steered anyway is answered after the turn.
  if (unfollowed(ctx)) return;
  const messages = await signalsOfAgent(ctx).progress(ctx.request.id);
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
export async function settleSteering(ctx: restate.SessionContext, conversation: Pick<Steerable, 'has'>) {
  const taken = steerings.get(ctx);
  if (!taken) return;
  for (const message of await signalsOfAgent(ctx).steering(ctx.request.id)) {
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
export const chatHandler = <A = string>(answer: (ctx: restate.SessionContext, message: string) => Promise<A>) => ({
  input: messageSerde,
  accept: '*/*',
  explicitCancellation: true,
  handler: async (ctx: restate.SessionContext, text: string): Promise<A> => {
    const invocationId = ctx.request.id;
    // The companion knows the turn from here to its close: `status`, `stop` and `steer` ask it.
    await ctx
      .rpc(signals, signalsOf(ctx))
      .start({ agent: agentOf(ctx), turn: { invocationId, since: new Date().toISOString() } });
    const taken = new Set<string>();
    steerings.set(ctx, taken);
    await stepped(ctx);
    const end = async (dropped = false) => {
      ctx
        .send(signals, signalsOf(ctx))
        .close({ agent: agentOf(ctx), turn: invocationId, taken: [...taken], cancelled: dropped });
      await publish(ctx);
    };
    const turn = new AbortController();
    turns.set(ctx, turn.signal);
    void (async () => {
      await cancelled(ctx);
      turn.abort(new restate.CancelledError());
      await cancelCalls(ctx);
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
});

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
  return /API key not found|no API key|Could not resolve authentication method|Provider is not configured|"code":"(AUTH|MISSING_CREDENTIAL)"/i.test(
    String(error),
  );
}

/**
 * `error` as a terminal error ({@link REFUSED}), if a retry does not change
 * it: a model the agent cannot take, or its provider's refusal.
 */
export function refusal(error: unknown): restate.TerminalError | undefined {
  if (error instanceof ModelError) return error;
  if (error instanceof restate.TerminalError || !refused(error)) return undefined;
  return new restate.TerminalError(`the model's provider refused the request: ${CHECK_MODEL}. ${String(error)}`, {
    errorCode: REFUSED,
  });
}

/**
 * A prompt's stored answer, as the prompt returns it: {@link UNANSWERED},
 * stored as its provider refused it, fails as the prompt did then
 * ({@link REFUSED}), also in a retry after a crash right after it was stored.
 */
export function storedAnswer(answer: string): string {
  if (answer !== UNANSWERED) return answer;
  throw new restate.TerminalError(`the model's provider refused the request: ${CHECK_MODEL}`, { errorCode: REFUSED });
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
 *
 * A message, once given out by `messages()`, is never changed: a change
 * makes a new one in its place. So a framework can keep its transcript and
 * add to it as its conversation grows, and the log stores only the messages
 * that are not the ones it stored last (see log.ts).
 */
export class Transcript {
  #messages: Said[] = [];
  /** The message of each tool call, by the call's id: its index. */
  #calls = new Map<string, number>();

  /** A new message of `role`, with `text` if there is any. */
  say(role: ChatMessage['role'], text = '') {
    this.#messages.push({ role, parts: text.trim() ? [{ type: 'text', text }] : [] });
  }

  /** Tool call `id` of the assistant's message. */
  call(id: string, name: string, input: unknown) {
    if (this.#messages.at(-1)?.role !== 'assistant') this.say('assistant');
    const at = this.#messages.length - 1;
    this.#change(at, (parts) => [...parts, { type: 'tool', id, name, input }]);
    this.#calls.set(id, at);
  }

  /** The output of tool call `id`; an error's if the framework says so, or it is a tool's `Error: <message>`. */
  result(id: string, output: string, error = false) {
    const at = this.#calls.get(id);
    if (at === undefined) return;
    const failed = error || output.startsWith('Error: ');
    this.#change(at, (parts) =>
      parts.map((p) => (p.type === 'tool' && p.id === id ? { ...p, output, ...(failed ? { error: true } : {}) } : p)),
    );
  }

  /** Whether tool call `id` has its output, and not an error's. */
  answered(id: string) {
    const at = this.#calls.get(id);
    return (
      at !== undefined &&
      this.#messages[at]!.parts.some((p) => p.type === 'tool' && p.id === id && p.output !== undefined && !p.error)
    );
  }

  /** Its messages, in order: those with parts. */
  messages(): Said[] {
    return this.#messages.filter((m) => m.parts.length > 0);
  }

  /** Message `at` with its parts as `change` makes them: a new message, in its place. */
  #change(at: number, change: (parts: Part[]) => Part[]) {
    const message = this.#messages[at]!;
    this.#messages[at] = { ...message, parts: change(message.parts) };
  }
}

/**
 * A framework's transcript as it stands, while it answers (see
 * {@link transcribing}): a new one, or the one it keeps and adds to.
 */
export type Read = () => Promise<Transcript>;

/** The transcript of the turn of each context, while it answers. */
const reading = new WeakMap<object, Read>();

/** The log writes of each context, one after another: tool calls in parallel publish at once. */
const writing = new WeakMap<object, Promise<void>>();

/** The log of the conversation of `ctx` takes its transcript as `read` gives it. */
function logged(ctx: restate.SessionContext, read: Read): Promise<void> {
  const write = async () => {
    const transcript = await read();
    await log.sync(ctx.db, transcript.messages());
    // The calls whose results the framework stored are made: see calls.ts.
    await settle(ctx, (id) => transcript.answered(id));
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
export async function transcribing<T>(ctx: restate.SessionContext, read: Read, work: () => Promise<T>): Promise<T> {
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
 * (`{cursor}`). It is a view: it answers while a message is answered, with
 * what was stored so far, and stores nothing.
 */
export const messagesHandler =
  () =>
  async (ctx: restate.ViewContext, request?: MessagesRequest): Promise<Messages> =>
    log.page(ctx.db, request ?? {});
