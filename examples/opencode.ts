// A durable chat on OpenCode 2.0, embedded: its sessions and events in the
// object's SQL database. A crash resumes the reply.
//
//   ANTHROPIC_API_KEY=… node opencode.ts
//   curl localhost:8080/session/alice/chat --json '"Hi, I am Alice"'
import * as restate from '@restatedev/restate-sdk';
import * as opencode from 'restate-agents/opencode';

const session = restate.object({ name: 'session', handlers: { chat: opencode.chat() }, options: restate.sqlOptions });

await restate.serve({ services: [session] });
