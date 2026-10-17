// What the integrations share.
import * as restate from '@restatedev/restate-sdk';

/** A value, or what it is for an invocation. */
export type PerCall<T> = T | ((ctx: restate.SqlObjectContext) => T);

/** `value` for the invocation of `ctx`. */
export const forCall = <T>(value: PerCall<T>, ctx: restate.SqlObjectContext): T =>
  typeof value === 'function' ? (value as (ctx: restate.SqlObjectContext) => T)(ctx) : value;

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
