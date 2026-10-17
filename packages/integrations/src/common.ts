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

/**
 * The model id of `model`, for a framework with only `provider`'s models:
 * `provider/model-id`, or the id alone.
 */
export const modelIdOf = (model: string, provider: string) => {
  const [named, id] = model.includes('/') ? model.split('/', 2) : [provider, model];
  if (named !== provider || !id) throw new Error(`${model}: not a model of ${provider} (${provider}/<model-id>)`);
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
  restate.handlers.object.exclusive({ input: message, accept: '*/*', journalReplay: false }, answer);

/** A message of a conversation, as an agent's `messages` handler returns it. */
export interface ChatMessage {
  role: 'user' | 'assistant';
  text: string;
}

/**
 * `read` as the handler of an agent's conversation: what was said, in order,
 * without the tool calls and results. It is exclusive, as `ctx.db` is: it
 * waits for a message being answered.
 */
export const messagesHandler = (read: (ctx: restate.SqlObjectContext) => Promise<ChatMessage[]>) =>
  restate.handlers.object.exclusive({ journalReplay: false }, read);

/** The non-empty `text`s as `role`'s messages. */
export const said = (role: ChatMessage['role'], text: string): ChatMessage[] => (text.trim() ? [{ role, text }] : []);
