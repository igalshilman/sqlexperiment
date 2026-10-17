// A durable chat on Claude (the Anthropic SDK's tool runner): the
// conversation in the object's SQL database. A crash resumes the reply.
//
//   ANTHROPIC_API_KEY=… node claude.ts
//   curl localhost:8080/session/alice/chat --json '"Hi, I am Alice"'
import * as restate from '@restatedev/restate-sdk';
import * as claude from 'restate-agents/claude';

const session = restate.object({ name: 'session', handlers: { chat: claude.chat() }, options: restate.sqlOptions });

await restate.serve({ services: [session] });
