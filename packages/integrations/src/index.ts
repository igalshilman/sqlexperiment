// restate-agents: agent frameworks in Restate virtual objects. Each framework
// is a subpath (restate-agents/pi, /opencode, /langgraph, /claude,
// /deepseek); this is what they share: virtual objects with SQL, tools for
// any of them, and the tools that are Restate itself.
import * as restate from '@restatedev/restate-sdk';
import { signals } from './signals.ts';

export {
  chatHandler,
  type ChatMessage,
  type Messages,
  type MessagesRequest,
  type Part,
  type PerCall,
} from './common.ts';
export { object } from './object.ts';
export { signals } from './signals.ts';
export { call, send, signal, subagent, tool, workflow, type Tool, type ToolContext } from './tools.ts';

/**
 * Serve `services` on PORT (default 9080), with what the agents need beside
 * them (the `signals` object).
 */
export const serve = (...services: restate.ServeOptions['services']) =>
  restate.serve({ services: [...services, signals], port: Number(process.env.PORT ?? 9080) });
