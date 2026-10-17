// An agent's client, for a chat UI or any other code: its handlers through
// Restate's ingress, over fetch, with their types. It imports nothing but
// types, so a web app bundles it without the SDK.
//
//   import { agentClient } from 'restate-agents/client';
//
//   const alice = agentClient('http://localhost:8080', 'assistant', 'alice');
//   const { invocationId } = await alice.send('Plan my week');
//   const { messages } = await alice.messages();     // as it works
//   const answer = await alice.attach(invocationId);  // once it answered
import type { ChatMessage, Messages, MessagesRequest, Part, Turn } from './common.ts';
import type { Status } from './object.ts';
import type { Resolved, Signal, Waiting } from './signals.ts';

export type { ChatMessage, Messages, MessagesRequest, Part, Resolved, Signal, Status, Turn, Waiting };

/** A failed request: the ingress's HTTP status (424 when the model's provider refused), and its message. */
export class AgentError extends Error {
  readonly status: number;
  constructor(status: number, message: string) {
    super(message);
    this.name = 'AgentError';
    this.status = status;
  }
}

/** Options of a message: an idempotency key makes sending it again the same message. */
export interface SendOptions {
  idempotencyKey?: string;
}

/** The handlers of the agent `name`'s key `key` (see {@link agentClient}). */
export interface AgentClient {
  /** Send `message`, and wait for the answer. */
  chat(message: string, options?: SendOptions): Promise<string>;
  /** Send `message` without waiting: its invocation's id, for {@link attach}. */
  send(message: string, options?: SendOptions): Promise<{ invocationId: string }>;
  /** Wait for the answer of the message sent as invocation `invocationId`. */
  attach(invocationId: string): Promise<string>;
  /** The conversation, a page at a time; it answers while a message is answered too. */
  messages(request?: MessagesRequest): Promise<Messages>;
  /** What the agent is doing: the message it answers, and what its tools wait for. */
  status(): Promise<Status>;
  /** What the agent's tools wait for. */
  pending(): Promise<Waiting[]>;
  /** Resolve a wait: by its id, or the oldest for a name. */
  signal(signal: Signal): Promise<Resolved>;
}

/** The headers of `options`. */
const keyed = ({ idempotencyKey }: SendOptions = {}): Record<string, string> =>
  idempotencyKey === undefined ? {} : { 'idempotency-key': idempotencyKey };

/**
 * The client of the agent (any `object()`) `name`, key `key`, through
 * Restate's ingress at `url` (such as http://localhost:8080). `headers` go
 * with every request (an API key of your ingress, say).
 */
export function agentClient(
  url: string,
  name: string,
  key: string,
  { headers = {} }: { headers?: Record<string, string> } = {},
): AgentClient {
  const base = `${url.replace(/\/$/, '')}/${encodeURIComponent(name)}/${encodeURIComponent(key)}`;

  const request = async <T>(
    path: string,
    init: { method?: string; headers?: Record<string, string>; body?: string } = {},
  ): Promise<T> => {
    const response = await fetch(path, { ...init, headers: { ...headers, ...init.headers } });
    const text = await response.text();
    if (!response.ok) {
      let message = text;
      try {
        message = (JSON.parse(text) as { message?: string }).message ?? text;
      } catch {
        // Not JSON: the text is the message.
      }
      throw new AgentError(response.status, message);
    }
    return (text ? JSON.parse(text) : null) as T;
  };

  const post = <T>(handler: string, body?: unknown, extra: Record<string, string> = {}) =>
    request<T>(`${base}/${handler}`, {
      method: 'POST',
      headers: { 'content-type': 'application/json', ...extra },
      body: JSON.stringify(body ?? null),
    });

  return {
    chat: (message, options) => post<string>('chat', message, keyed(options)),
    send: (message, options) => post<{ invocationId: string }>('chat/send', message, keyed(options)),
    attach: (invocationId) =>
      request<string>(`${url.replace(/\/$/, '')}/restate/invocation/${encodeURIComponent(invocationId)}/attach`),
    messages: (messagesRequest) => post<Messages>('messages', messagesRequest),
    status: () => post<Status>('status'),
    pending: () => post<Waiting[]>('pending'),
    signal: (signal) => post<Resolved>('signal', signal),
  };
}
