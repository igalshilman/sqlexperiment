// What the integrations share.
import * as restate from '@restatedev/restate-sdk';

/** A value, or what it is for an invocation. */
export type PerCall<T> = T | ((ctx: restate.SqlObjectContext) => T);

/** `value` for the invocation of `ctx`. */
export const forCall = <T>(value: PerCall<T>, ctx: restate.SqlObjectContext): T =>
  typeof value === 'function' ? (value as (ctx: restate.SqlObjectContext) => T)(ctx) : value;

/**
 * The model, as `provider/model-id` (such as `anthropic/claude-sonnet-5-5`):
 * `model`, or else MODEL, or else the framework's default.
 */
export const modelOf = (model: string | undefined, fallback: string) => model || process.env.MODEL || fallback;

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
const message: restate.Serde<string> = {
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
 * `answer` as a chat handler, which takes its message as plain text or as a
 * JSON string, so that both of these work:
 *
 *   curl localhost:8080/assistant/alice/chat -d 'Hi'
 *   curl localhost:8080/assistant/alice/chat --json '"Hi"'
 */
export const chatHandler = (answer: (ctx: restate.SqlObjectContext, message: string) => Promise<string>) =>
  restate.handlers.object.exclusive(
    { input: message, accept: '*/*', journalReplay: false },
    async (ctx: restate.SqlObjectContext, text: string) => {
      try {
        return await answer(ctx, text);
      } catch (error) {
        if (error instanceof restate.TerminalError || !refused(error)) throw error;
        throw new restate.TerminalError(`the model's provider refused the request: ${CHECK_MODEL}. ${String(error)}`);
      }
    },
  );

/** The Anthropic SDK's errors of a request that fails again however often it is made. */
const FOR_GOOD = new Set(['AuthenticationError', 'PermissionDeniedError', 'NotFoundError', 'BadRequestError']);

/**
 * Whether `error` is a model provider's refusal that a retry does not change:
 * a missing or wrong API key, an unknown model, a bad request. Other errors
 * (a tool's, an overloaded provider's) fail the attempt, and Restate retries.
 */
function refused(error: unknown) {
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

/** What a message says: text, or a tool call with its output once it ran. */
export type Part =
  | { type: 'text'; text: string }
  | { type: 'tool'; id: string; name: string; input: unknown; output?: string };

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

  /** The output of tool call `id`. */
  result(id: string, output: string) {
    const call = this.#calls.get(id);
    if (call) call.output = output;
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
 * was said, in order, with the tool calls. It is exclusive, as `ctx.db` is:
 * it waits for a message being answered.
 */
export const messagesHandler = (read: (ctx: restate.SqlObjectContext, transcript: Transcript) => Promise<void>) =>
  restate.handlers.object.exclusive(
    { journalReplay: false },
    async (ctx: restate.SqlObjectContext, request?: MessagesRequest) => {
      const transcript = new Transcript();
      await read(ctx, transcript);
      return transcript.page(request ?? {});
    },
  );
