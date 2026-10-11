// The scripted teller: a LangChain chat model that needs no API key and
// answers deterministically, so the agent example and the crash tests run
// offline. It transfers and audits on request, and reports the tool's result.
//
// Any LangChain chat model that calls tools takes its place, such as
// `new ChatAnthropic({ model: 'claude-sonnet-4-5' })` from @langchain/anthropic.
import { BaseChatModel } from '@langchain/core/language_models/chat_models';
import { AIMessage } from '@langchain/core/messages';

const textOf = (content) =>
  typeof content === 'string'
    ? content
    : content
        .filter((c) => c.type === 'text')
        .map((c) => c.text)
        .join('');

export class Teller extends BaseChatModel {
  _llmType() {
    return 'teller';
  }

  // It knows its tools.
  bindTools() {
    return this;
  }

  /** The reply to `messages`. */
  reply(messages) {
    const last = messages.at(-1);
    const text = textOf(last.content);
    if (last.getType() === 'tool') return new AIMessage(`Done: ${text}`);
    // A tool call id, unique in the conversation, and the same when a step is
    // run again.
    const id = `call_${messages.length}`;
    const move = /move (\d+) from (\d+) to (\d+)/i.exec(text);
    if (move) {
      const [amount, from, to] = move.slice(1).map(Number);
      return new AIMessage({ content: 'On it.', tool_calls: [{ id, name: 'transfer', args: { from, to, amount } }] });
    }
    if (/audit/i.test(text)) return new AIMessage({ content: '', tool_calls: [{ id, name: 'audit', args: {} }] });
    return new AIMessage('Ask me to "move 5 from 1 to 2", or to "audit" the books.');
  }

  async _generate(messages) {
    const message = this.reply(messages);
    return { generations: [{ text: textOf(message.content), message }] };
  }
}
