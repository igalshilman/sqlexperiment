// A durable chat on LangGraph: its checkpoints in the object's SQL
// database. A crash resumes the reply.
//
//   ANTHROPIC_API_KEY=… node langgraph.ts
//   curl localhost:8080/session/alice/chat --json '"Hi, I am Alice"'
import * as restate from '@restatedev/restate-sdk';
import * as langgraph from 'restate-agents/langgraph';

const session = restate.object({ name: 'session', handlers: { chat: langgraph.chat() }, options: restate.sqlOptions });

await restate.serve({ services: [session] });
