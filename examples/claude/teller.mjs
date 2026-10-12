// The scripted teller: an Anthropic client whose `fetch` answers the Messages
// API itself, so the agent example and the crash tests run without an API key.
// It answers deterministically: it transfers and audits on request, and
// reports the tool's result.
//
// `clientFromEnv()` gives the teller, or a real model with
// CLAUDE_MODEL=claude-sonnet-5-5 (and ANTHROPIC_API_KEY).
import Anthropic from '@anthropic-ai/sdk';

const textOf = (content) =>
  typeof content === 'string'
    ? content
    : content
        .map((c) => (c.type === 'text' ? c.text : c.type === 'tool_result' ? textOf(c.content ?? '') : ''))
        .join('');

/** The teller's reply to `messages`, as a Messages API response. */
export function reply(messages) {
  const last = messages.at(-1);
  const text = textOf(last.content);
  const message = (content) => ({
    id: `msg_${messages.length}`,
    type: 'message',
    role: 'assistant',
    model: 'teller',
    content,
    stop_reason: content.some((c) => c.type === 'tool_use') ? 'tool_use' : 'end_turn',
    stop_sequence: null,
    usage: { input_tokens: 0, output_tokens: 0 },
  });
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

/** A client of the teller. `teller.reply` can be replaced, to count or change replies. */
export const teller = {
  reply,
  client: () =>
    new Anthropic({
      apiKey: 'teller',
      maxRetries: 0,
      fetch: async (_url, init) =>
        new Response(JSON.stringify(teller.reply(JSON.parse(init.body).messages)), {
          headers: { 'content-type': 'application/json' },
        }),
    }),
};

/** `{ client, model }`: the teller, or Claude with CLAUDE_MODEL. */
export const clientFromEnv = () =>
  process.env.CLAUDE_MODEL
    ? { client: new Anthropic(), model: process.env.CLAUDE_MODEL }
    : { client: teller.client(), model: 'teller' };
