// A durable chat: an agent (pi) per key, its conversation in the object's
// SQL database. A crash resumes the reply.
//
//   ANTHROPIC_API_KEY=… node chat.ts
//   curl localhost:8080/session/alice/chat --json '"Hi, I am Alice"'
//   curl localhost:8080/session/alice/chat --json '"What is my name?"'
import * as restate from '@restatedev/restate-sdk';
import * as pi from 'restate-agents/pi';

const session = restate.object({ name: 'session', handlers: { chat: pi.chat() }, options: restate.sqlOptions });

await restate.serve({ services: [session] });
