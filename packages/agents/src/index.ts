// restate-agents: agent frameworks in Restate virtual objects. Each framework
// is a subpath (restate-agents/pi, /opencode, /langgraph, /claude,
// /deepseek); this is what they share: virtual objects with SQL, tools for
// any of them, and the tools that are Restate itself.
import { sdk as restate } from 'restate-objects/internal';
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
 * them (the `signals` object). RESTATE_SERVE_ONLY, names separated by commas
 * (`triage,planner,signals`), serves only those of them: one image, deployed
 * in parts. The agents reach `signals` through Restate, so one part serves it
 * for them all.
 */
export const serve = (...services: restate.ServeOptions['services']) =>
  restate.serve({
    services: only([...services.filter((s) => s !== signals), signals], process.env.RESTATE_SERVE_ONLY),
    port: Number(process.env.PORT ?? 9080),
  });

/** The `services` that `names` (comma separated) names, or all of them without names; a name of none is an error. */
function only(services: restate.ServeOptions['services'], names: string | undefined) {
  const wanted = (names ?? '')
    .split(',')
    .map((name) => name.trim())
    .filter(Boolean);
  if (wanted.length === 0) return services;
  const known = services.map((s) => s.name);
  const unknown = wanted.filter((name) => !known.includes(name));
  if (unknown.length > 0)
    throw new Error(`RESTATE_SERVE_ONLY names ${unknown.join(', ')}: the services are ${known.join(', ')}`);
  return services.filter((s) => wanted.includes(s.name));
}
