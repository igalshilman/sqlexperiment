// A durable chat on the DeepSeek Harness: its session's event log in the
// object's SQL database. A crash resumes the reply.
//
//   DEEPSEEK_API_KEY=… node deepseek.ts
//   curl localhost:8080/session/alice/chat --json '"Hi, I am Alice"'
import * as restate from '@restatedev/restate-sdk';
import * as deepseek from 'restate-agents/deepseek';

const session = restate.object({ name: 'session', handlers: { chat: deepseek.chat() }, options: restate.sqlOptions });

await restate.serve({ services: [session] });
