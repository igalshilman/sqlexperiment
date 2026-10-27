// Test only: the scripted teller, an Anthropic client whose `fetch` answers
// the Messages API itself, so the agent's tests and the crash test run without
// an API key. It answers deterministically: it transfers and audits on
// request, and reports the tool's result.
//
// The module stands in for @anthropic-ai/sdk (see fakes.ts): its default
// export is a client of the teller, whatever its options.
import { Anthropic } from '@anthropic-ai/sdk/client';

type MessageParam = Anthropic.Beta.Messages.BetaMessageParam;
type Message = Anthropic.Beta.Messages.BetaMessage;

/** The text of a message's content, and of the tool results in it. */
const textOf = (
  content: MessageParam['content'] | Anthropic.Beta.Messages.BetaToolResultBlockParam['content'],
): string =>
  typeof content === 'string'
    ? content
    : (content ?? [])
        .map((c) => (c.type === 'text' ? c.text : c.type === 'tool_result' ? textOf(c.content) : ''))
        .join('');

/** The teller's reply to `messages`, as a Messages API response. */
export function reply(messages: MessageParam[]): Message {
  const last = messages.at(-1)!;
  const text = textOf(last.content);
  const message = (content: object[]): Message =>
    ({
      id: `msg_${messages.length}`,
      type: 'message',
      role: 'assistant',
      model: 'teller',
      content,
      stop_reason: content.some((c) => 'name' in c) ? 'tool_use' : 'end_turn',
      stop_sequence: null,
      usage: { input_tokens: 0, output_tokens: 0 },
    }) as Message;
  if (Array.isArray(last.content) && last.content.some((c) => c.type === 'tool_result')) {
    return message([{ type: 'text', text: `Done: ${text}` }]);
  }
  // A tool call id, unique in the conversation, and the same when a step is
  // run again.
  const id = `toolu_${messages.length}`;
  const move = /move (\d+) from (\d+) to (\d+)/i.exec(text);
  if (move) {
    const [amount, from, to] = move.slice(1).map(Number);
    return message([
      { type: 'text', text: 'On it.' },
      { type: 'tool_use', id, name: 'transfer', input: { from, to, amount } },
    ]);
  }
  if (/audit/i.test(text)) return message([{ type: 'tool_use', id, name: 'audit', input: {} }]);
  return message([{ type: 'text', text: 'Ask me to "move 5 from 1 to 2", or to "audit" the books.' }]);
}

/** An error response of the Messages API, for a replaced `teller.reply` to throw. */
export class HttpError extends Error {
  readonly status: number;
  readonly type: string;

  constructor(status: number, type: string, message: string) {
    super(message);
    this.status = status;
    this.type = type;
  }
}

/** The teller's replies. `teller.reply` can be replaced, to count or change replies, or see each request. */
export const teller: { reply: (messages: MessageParam[], request?: Record<string, unknown>) => Message } = { reply };

/** An Anthropic client of the teller. */
export default class TellerClient extends Anthropic {
  constructor(options: ConstructorParameters<typeof Anthropic>[0] = {}) {
    super({
      ...options,
      apiKey: 'teller',
      maxRetries: 0,
      fetch: async (_url, init) => {
        if (typeof init?.body !== 'string') throw new Error('the teller reads JSON request bodies');
        const headers = { 'content-type': 'application/json' };
        try {
          const request = JSON.parse(init.body);
          return new Response(JSON.stringify(teller.reply(request.messages, request)), { headers });
        } catch (error) {
          // A reply that throws an HttpError is that response of the API.
          if (!(error instanceof HttpError)) throw error;
          const body = { type: 'error', error: { type: error.type, message: error.message } };
          return new Response(JSON.stringify(body), { status: error.status, headers });
        }
      },
    });
  }
}
