// A durable chat: a session object per conversation, whose agent (pi) keeps
// the conversation in the object's SQL database. A crash resumes the reply.
//
//   ANTHROPIC_API_KEY=… node chat/chat.ts
//   curl localhost:8080/session/alice/chat --json '"Hi, I am Alice"'
//   curl localhost:8080/session/alice/chat --json '"What is my name?"'
import * as restate from '@restatedev/restate-sdk';
import { prompt } from '../pi/with-pi.ts';

export const session = restate.object({
  name: 'session',
  handlers: {
    chat: (ctx: restate.SqlObjectContext, message: string) => prompt(ctx, message),
  },
  options: restate.sqlOptions,
});

if (import.meta.main) await restate.serve({ services: [session] });
