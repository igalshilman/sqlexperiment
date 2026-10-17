// Test only: fake pi-ai models, whose replies a function computes from the
// conversation, so that an agent runs without an API key, deterministically.
//
//   vi.mock('@earendil-works/pi-ai/providers/anthropic', () => ({
//     anthropicProvider: () => fakeProvider('anthropic', 'claude-sonnet-5-5', (context) => say('hi')),
//   }));
//
// `respond(context)` gets each model call's context (its `messages`) and
// returns the assistant message, built with `say` or `callTools`. Every model
// call is answered, however many there are, which matters when a retried
// attempt makes a model call again.
import type { AssistantMessage, Message, Provider, ToolCall, TranscriptContext } from '@earendil-works/pi-ai';
import { fauxAssistantMessage, fauxProvider, fauxText, fauxToolCall } from '@earendil-works/pi-ai/providers/faux';

/** A model's reply to the conversation in `context`. */
export type Respond = (context: TranscriptContext) => AssistantMessage;

/** A pi-ai provider `provider`, with one model `model`, answering with `respond`. */
export function fakeProvider(provider: string, model: string, respond: Respond): Provider {
  const faux = fauxProvider({ provider, models: [{ id: model }] });
  // The faux provider answers from a queue: each reply queues the next.
  const reply = (context: TranscriptContext): AssistantMessage => {
    faux.appendResponses([reply]);
    return respond(context);
  };
  faux.setResponses([reply]);
  return faux.provider;
}

/** The text of a message's content. */
export const textOf = (content: Message['content']): string =>
  typeof content === 'string'
    ? content
    : content
        .filter((c) => c.type === 'text')
        .map((c) => c.text)
        .join('');

/** The text of the conversation's last message. */
export const lastText = (context: TranscriptContext) => textOf(context.messages.at(-1)!.content);

/** A reply in text. */
export const say = (text: string) => fauxAssistantMessage(fauxText(text));

/** A reply that calls tools: `calls` is a list of [name, arguments], after an optional text. */
export const callTools = (calls: [name: string, args: ToolCall['arguments']][], text?: string) =>
  fauxAssistantMessage([...(text ? [fauxText(text)] : []), ...calls.map(([name, args]) => fauxToolCall(name, args))], {
    stopReason: 'toolUse',
  });
