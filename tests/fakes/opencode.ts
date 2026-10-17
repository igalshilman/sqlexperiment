// Test only: the scripted teller, an AI SDK language model that OpenCode
// calls like any other, and that needs no API key and answers
// deterministically, so the agent's tests and the crash test run offline. It
// transfers and audits on request, and reports the tool's result.
//
// When OpenCode resumes a turn that a crash interrupted, a tool call that was
// running gets an "interrupted" result, and OpenCode asks the model to
// continue. The teller then retries such a transfer with `retryOf` naming the
// call, so it moves the money once; reports a result that came before the
// crash; and otherwise answers the prompt again.
//
// It is the model `teller/teller` of OpenCode booted with `tellerPlugin`, as
// `withTeller(openOpenCode)` does: with OPENCODE_MODEL=teller/teller, an
// agent's model.
import { randomUUID } from 'node:crypto';
import type {
  LanguageModelV3,
  LanguageModelV3CallOptions,
  LanguageModelV3Message,
  LanguageModelV3StreamPart,
} from '@ai-sdk/provider';
import { Model, Provider, type Plugin } from '@opencode/plugin';
import type { openOpenCode } from 'restate-agents/opencode/db';

/** A reply of the teller: text, and maybe a tool call. */
export interface Reply {
  text?: string;
  call?: { id: string; name: string; input: Record<string, unknown> };
}

/** The text of a message, and of the tool results in it. */
const textOf = (message: LanguageModelV3Message): string =>
  typeof message.content === 'string'
    ? message.content
    : message.content
        .map((part) => {
          if (part.type === 'text') return part.text;
          if (part.type !== 'tool-result') return '';
          const { output } = part;
          if (output.type === 'text' || output.type === 'error-text') return output.value;
          return 'value' in output ? JSON.stringify(output.value) : output.type;
        })
        .join('');

/** How OpenCode asks the model to go on with a turn a restart interrupted. */
const CONTINUE = /^The server restarted while you were working/;
/** A tool result of a call a restart interrupted. */
const INTERRUPTED = /Tool execution interrupted/;

/** The teller's reply to the conversation `prompt`, offering `tools`. */
export function reply(prompt: LanguageModelV3Message[], tools: string[]): Reply {
  // OpenCode also asks for a session title, offering no tools.
  if (tools.length === 0) return { text: 'Teller' };
  const last = prompt.at(-1)!;
  if (last.role === 'tool') return { text: `Done: ${textOf(last)}` };
  // A fresh tool call id, unique like a model's: a step run again calls its
  // tools with new ids.
  const id = `call_${randomUUID()}`;
  let text = textOf(last);
  if (CONTINUE.test(text)) {
    // The interrupted prompt, and the last tool result since.
    const at = prompt.findLastIndex((m) => m.role === 'user' && !CONTINUE.test(textOf(m)));
    const result = prompt.slice(at).findLast((m) => m.role === 'tool');
    const part = result?.role === 'tool' ? result.content.findLast((c) => c.type === 'tool-result') : undefined;
    if (result && part && INTERRUPTED.test(textOf(result))) {
      const call = prompt
        .flatMap((m) => (m.role === 'assistant' ? m.content : []))
        .findLast((c) => c.type === 'tool-call' && c.toolCallId === part.toolCallId);
      if (call?.type !== 'tool-call') throw new Error(`no tool call ${part.toolCallId}`);
      const input = { ...(call.input as Record<string, unknown>) };
      // OpenCode stores a tool call when its input starts: one a crash
      // interrupted before the input was complete never ran, so the prompt
      // is answered again.
      if (call.toolName !== 'transfer' || 'amount' in input) {
        if (call.toolName === 'transfer') input.retryOf ??= call.toolCallId;
        return { call: { id, name: call.toolName, input } };
      }
    } else if (result) {
      return { text: `Done: ${textOf(result)}` };
    }
    text = textOf(prompt[at]!);
  }
  const move = /move (\d+) from (\d+) to (\d+)/i.exec(text);
  if (move) {
    const [amount, from, to] = move.slice(1).map(Number);
    return { text: 'On it.', call: { id, name: 'transfer', input: { from, to, amount } } };
  }
  if (/audit/i.test(text)) return { call: { id, name: 'audit', input: {} } };
  return { text: 'Ask me to "move 5 from 1 to 2", or to "audit" the books.' };
}

const none = undefined;
/** `reply` as a model's stream. */
function stream({ text, call }: Reply): LanguageModelV3StreamPart[] {
  const parts: LanguageModelV3StreamPart[] = [{ type: 'stream-start', warnings: [] }];
  if (text !== undefined) {
    parts.push({ type: 'text-start', id: 'text' }, { type: 'text-delta', id: 'text', delta: text });
    parts.push({ type: 'text-end', id: 'text' });
  }
  if (call) {
    parts.push({ type: 'tool-call', toolCallId: call.id, toolName: call.name, input: JSON.stringify(call.input) });
  }
  const reason = call ? 'tool-calls' : 'stop';
  parts.push({
    type: 'finish',
    finishReason: { unified: reason, raw: reason },
    usage: {
      inputTokens: { total: 0, noCache: none, cacheRead: none, cacheWrite: none },
      outputTokens: { total: 0, text: none, reasoning: none },
    },
  });
  return parts;
}

/** The teller as a model. `teller.reply` can be replaced, to count or change replies. */
export const teller = {
  reply,
  model: {
    specificationVersion: 'v3',
    provider: 'teller',
    modelId: 'teller',
    supportedUrls: {},
    doGenerate: () => Promise.reject(new Error('the teller only streams')),
    doStream: async (options: LanguageModelV3CallOptions) => {
      const parts = stream(
        teller.reply(
          options.prompt,
          (options.tools ?? []).map((tool) => tool.name),
        ),
      );
      return {
        stream: new ReadableStream<LanguageModelV3StreamPart>({
          start(controller) {
            for (const part of parts) controller.enqueue(part);
            controller.close();
          },
        }),
      };
    },
  } satisfies LanguageModelV3,
};

const providerID = Provider.ID.make('teller');
// OpenCode makes an AI SDK model in two steps: it loads the provider's
// package, then makes the model, which a plugin's `language` hook may replace.
// It loads only packages it bundles, so the provider names one of them, and
// the hook replaces the model it makes, whose base URL does not resolve.
const CARRIER = 'aisdk:@ai-sdk/vercel';

/** An OpenCode plugin: a `teller` provider, whose one model is the teller. */
export const tellerPlugin: Plugin.Plugin = {
  id: 'examples.teller',
  async setup(ctx) {
    const provider = await ctx.provider.transform((editor) => {
      editor.add({
        info: {
          id: providerID,
          name: 'Teller',
          activation: 'enabled',
          package: CARRIER,
          settings: { baseURL: 'https://teller.invalid/v1', apiKey: 'teller' },
        },
        models: [
          {
            ...Model.Info.default(providerID, Model.ID.make('teller')),
            package: CARRIER,
            limit: { context: 128_000, output: 8_192 },
          },
        ],
      });
    });
    const hook = await ctx.aisdk.hook(
      'language',
      (event) => {
        event.language = teller.model;
      },
      { providerID },
    );
    return async () => {
      await hook.dispose();
      await provider.dispose();
    };
  },
};

/** `open` (restate-agents/opencode/db's openOpenCode), booting OpenCode with the teller too. */
export const withTeller =
  (open: typeof openOpenCode): typeof openOpenCode =>
  (db, fence, options) =>
    open(db, fence, { ...options, plugins: [tellerPlugin, ...(options.plugins ?? [])] });
