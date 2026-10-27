// restate-agents: agent frameworks in Restate virtual objects. Each framework
// is a subpath (restate-agents/pi, /opencode, /langgraph, /claude,
// /deepseek); this is what they share: virtual objects with SQL, tools for
// any of them, and the tools that are Restate itself.
import * as restate from '@restatedev/restate-sdk';
import { signals } from './signals.ts';

export {
  CANCELLED,
  chatHandler,
  type ChatMessage,
  type Messages,
  type MessagesRequest,
  type Part,
  REFUSED,
  type Turn,
  type Configure,
} from './common.ts';
export { object, type Status, type Stopped } from './object.ts';
export {
  companionOf,
  deliver,
  signals,
  type Companion,
  type Resolved,
  type Signal,
  type Steer,
  type Steered,
  type Waiting,
} from './signals.ts';
export { call, send, signal, subagent, tool, workflow, type Tool, type ToolCall, type ToolContext } from './tools.ts';
export { webhook, webhookMessage, type WebhookAuth, type WebhookEvent, type WebhookOptions } from './webhook.ts';

/**
 * Serve `services` on PORT (default 9080), with what the agents need beside
 * them (the `signals` object).
 */
export const serve = (...services: restate.ServeOptions['services']) =>
  restate.serve({
    services: [...services.filter((s) => s !== signals), signals],
    port: Number(process.env.PORT ?? 9080),
  });
