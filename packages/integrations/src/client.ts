// An agent for Restate's client (@restatedev/restate-sdk-clients), from a
// chat UI or any other code: the agent's definition as the client takes it,
// its name and the types of its handlers, and the types of what they answer.
// It imports nothing but types, so a web app bundles it without the SDK.
//
//   import * as clients from '@restatedev/restate-sdk-clients';
//   import { agent } from 'restate-agents/client';
//
//   const restate = clients.connect({ url: 'http://localhost:8080' });
//   const alice = restate.objectClient(agent('assistant'), 'alice');
//   const { messages } = await alice.messages({ limit: 20 });
import type { VirtualObjectDefinition } from '@restatedev/restate-sdk-core';
import type { ChatMessage, Messages, MessagesRequest, Part, Turn } from './common.ts';
import type { Status, Stopped } from './object.ts';
import type { Resolved, Signal, Waiting } from './signals.ts';

export type { ChatMessage, Messages, MessagesRequest, Part, Resolved, Signal, Status, Stopped, Turn, Waiting };

/** The handlers of an agent (`agent()` of any framework), as Restate's client calls them. */
export interface Agent {
  /** Answer `message`. */
  chat(ctx: unknown, message: string): Promise<string>;
  /** The conversation, a page at a time, also while a message is answered. */
  messages(ctx: unknown, request?: MessagesRequest): Promise<Messages>;
  /** What the agent is doing: the message it answers, and what its tools wait for. */
  status(ctx: unknown): Promise<Status>;
  /** Stop the message it answers: cancel its invocation. */
  stop(ctx: unknown): Promise<Stopped>;
  /** What its tools wait for. */
  pending(ctx: unknown): Promise<Waiting[]>;
  /** Resolve a wait: by its id, or the oldest for a name. */
  signal(ctx: unknown, signal: Signal): Promise<Resolved>;
}

/**
 * The agent `name`, as Restate's client takes it: its name, typed with the
 * handlers of an agent (or `H`, an object of your own's).
 */
export const agent = <H = Agent, P extends string = string>(name: P) => ({ name }) as VirtualObjectDefinition<P, H>;
