// The scripted teller: a DeepSeek Harness model adapter that needs no API key
// and answers deterministically, so the agent example and the crash tests run
// offline. It transfers and audits on request, and reports the tool's result.
//
// After a crash the harness closes the interrupted turn: a tool call it
// recorded without a result gets an error result saying its outcome is
// unknown. Asked to continue, the teller retries such a transfer with
// `retryOf` naming the call, so it moves the money once; reports a result
// that came before the crash; and otherwise answers the prompt again.
import { randomUUID } from 'node:crypto';
import {
  LlmAdapter,
  ToolCallId,
  type GenerateOptions,
  type RequestMessage,
  type StreamChunk,
  type ToolCallBlock,
} from '@deepseek-ai/dsh-llm';

const textOf = (message: RequestMessage) =>
  message.content.flatMap((c) => (c.type === 'text' ? [c.text] : [])).join('');

/** A reply of the teller: text, or a tool call. */
export type Reply =
  | { text: string; call?: undefined }
  | { text?: undefined; call: { id: ToolCallId; name: string; args: Record<string, unknown> } };

/** The text that asks the agent to finish an interrupted prompt. */
export const CONTINUE = 'Continue: your last turn was interrupted. Finish the request.';

/** The teller's reply to `messages`: `{ text }`, or `{ call: { id, name, args } }`. */
export function reply(messages: RequestMessage[]): Reply {
  const last = messages.at(-1)!;
  if (last.role === 'tool') return { text: `Done: ${textOf(last)}` };
  // A fresh tool call id, unique like a model's: a step run again calls its
  // tools with new ids.
  const id = ToolCallId(`call_${randomUUID()}`);
  let text = textOf(last);
  if (text === CONTINUE) {
    // The interrupted prompt, and the last tool result since.
    const at = messages.findLastIndex((m) => m.role === 'user' && textOf(m) !== CONTINUE);
    const result = messages.slice(at).findLast((m) => m.role === 'tool');
    if (result && /outcome is unknown/.test(textOf(result))) {
      const call = messages
        .flatMap((m) => (m.role === 'assistant' ? m.content : []))
        .findLast((c): c is ToolCallBlock => c.type === 'tool-call' && c.id === result.toolCallId);
      if (!call) throw new Error(`no tool call ${result.toolCallId}`);
      const args = JSON.parse(call.arguments);
      if (call.name === 'transfer') args.retryOf = args.retryOf ?? call.id;
      return { call: { id, name: call.name, args } };
    }
    if (result && !result.isError) return { text: `Done: ${textOf(result)}` };
    text = textOf(messages[at]!);
  }
  const move = /move (\d+) from (\d+) to (\d+)/i.exec(text);
  if (move) {
    const [amount, from, to] = move.slice(1).map(Number);
    return { call: { id, name: 'transfer', args: { from, to, amount } } };
  }
  if (/audit/i.test(text)) return { call: { id, name: 'audit', args: {} } };
  return { text: 'Ask me to "move 5 from 1 to 2", or to "audit" the books.' };
}

/** The teller as a model adapter, for the `teller` provider. `teller.reply` can be replaced, to count or change replies. */
export class Teller extends LlmAdapter {
  reply: (messages: RequestMessage[]) => Reply | Promise<Reply> = reply;

  async *stream({ messages }: GenerateOptions): AsyncIterable<StreamChunk> {
    const { text, call } = await this.reply(messages);
    if (call !== undefined) {
      const args = JSON.stringify(call.args);
      yield { type: 'block-start', index: 0, blockType: 'tool-call' };
      yield { type: 'tool-call-delta', index: 0, id: call.id, name: call.name, argumentsDelta: args };
      yield {
        type: 'block-end',
        index: 0,
        block: { type: 'tool-call', id: call.id, name: call.name, arguments: args },
      };
    } else if (text !== undefined) {
      yield { type: 'block-start', index: 0, blockType: 'text' };
      yield { type: 'text-delta', index: 0, text };
      yield { type: 'block-end', index: 0, block: { type: 'text', text } };
    }
    yield { type: 'usage', usage: { inputTokens: 0, outputTokens: 0 } };
    yield { type: 'finish', reason: { kind: call ? 'tool-calls' : 'stop' } };
  }
}

export const teller = new Teller();
