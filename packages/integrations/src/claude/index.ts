// Claude agents in Restate virtual objects, on the tool runner of the
// Anthropic SDK (`client.beta.messages.toolRunner`): the conversation is in
// the object's database (conversation.ts).
//
//   const assistant = claude.agent({ name: 'assistant', system: 'Answer briefly.', tools: [weather] });
//
// or a handler of an object of your own: `handlers: { chat: claude.chat() }`
//
// The model is `model`, or else MODEL, or else anthropic/claude-sonnet-5-5,
// with ANTHROPIC_API_KEY.
//
// The handler is not replayed: a retried attempt (a crash, a lost connection)
// runs again from the start, and starts the runner with the stored messages:
// - Each reply of the model is stored before its tools run, and their results
//   before the next request. So a crash during the tools resumes them with the
//   same tool call ids (`callId(context)`), without asking the model again,
//   and a tool passes the id on to stay idempotent.
// - A tool's error fails the attempt, which Restate retries (by default the
//   runner hands it to the model as the tool's result).
// - A message is answered once per invocation: its answer is stored with the
//   model's last reply, and a retried attempt returns it.
import type * as restate from '@restatedev/restate-sdk';
import Anthropic from '@anthropic-ai/sdk';
import type { BetaRunnableTool, BetaToolRunContext } from '@anthropic-ai/sdk/lib/tools/BetaRunnableTool';
import { chatHandler, forCall, modelIdOf, modelOf, type PerCall } from '../common.ts';
import { object } from '../object.ts';
import { betaZodTool } from '@anthropic-ai/sdk/helpers/beta/zod';
import { isTool, type Tool as AnyTool } from '../tools.ts';
import { Conversation } from './conversation.ts';

export { Conversation };

/** The default model. */
export const DEFAULT_MODEL = 'anthropic/claude-sonnet-5-5';

/** The id of the tool call a tool runs for: the runner always passes it. */
export const callId = (context?: BetaToolRunContext) => {
  if (!context) throw new Error('a tool call without its context');
  return context.toolUse.id;
};

/**
 * `tools`, whose errors fail the attempt: the runner would hand a thrown error
 * to the model as the tool's result, so each one is kept, for `check()` to
 * throw once the tools ran.
 */
function failing(tools: BetaRunnableTool[]) {
  let failure: unknown;
  return {
    tools: tools.map((tool): BetaRunnableTool => ({
      ...tool,
      run: async (args, context) => {
        try {
          return await tool.run(args, context);
        } catch (e) {
          failure ??= e;
          throw e;
        }
      },
    })),
    check: () => {
      if (failure !== undefined) throw failure;
    },
  };
}

const textOf = (content: Anthropic.Beta.Messages.BetaContentBlock[]) =>
  content
    .filter((c) => c.type === 'text')
    .map((c) => c.text)
    .join('');

interface Agent {
  client: Anthropic;
  model: string;
  system?: string;
  tools: BetaRunnableTool[];
}

/** Run prompt `id` of `conversation` until it is answered, from its stored messages. */
async function run({ client, model, system, tools: all }: Agent, conversation: Conversation, id: string) {
  const { tools, check } = failing(all);
  const runner = client.beta.messages.toolRunner({
    model,
    max_tokens: 4096,
    ...(system === undefined ? {} : { system }),
    tools,
    messages: await conversation.messages(),
  });
  // A reply whose tools did not finish (a crash): run them, with the same ids.
  if (runner.params.messages.at(-1)?.role === 'assistant') {
    const results = await runner.generateToolResponse();
    check();
    if (results) {
      await conversation.append(id, [results]);
      runner.pushMessages(results);
    }
  }
  for await (const message of runner) {
    const reply: Anthropic.Beta.Messages.BetaMessageParam = { role: 'assistant', content: message.content };
    if (!message.content.some((c) => c.type === 'tool_use')) {
      await conversation.append(id, [reply], textOf(message.content));
      return;
    }
    // The reply first, then its tools.
    await conversation.append(id, [reply]);
    const results = await runner.generateToolResponse();
    check();
    if (results) await conversation.append(id, [results]);
  }
}

/** `t` as a tool of the Anthropic SDK's tool runner. */
const native = (t: AnyTool, ctx: restate.SqlObjectContext): BetaRunnableTool =>
  betaZodTool({
    name: t.name,
    description: t.description,
    inputSchema: t.input,
    run: (input, context) => t.run(input, { ctx, callId: callId(context) }),
  });

export interface ChatOptions {
  /** The model, as `anthropic/model-id`: MODEL, or else {@link DEFAULT_MODEL}. */
  model?: string;
  /** The system prompt. */
  system?: string;
  /** The agent's tools: `tool()`s, Restate's (`call`, `signal`...), or the SDK's (such as `betaZodTool`s), or for an invocation. */
  tools?: (PerCall<BetaRunnableTool> | AnyTool)[];
}

/** The answer to a message, each message an invocation, the conversation in the object's database. */
const respond = ({ model: named, system, tools = [] }: ChatOptions = {}) => {
  const client = new Anthropic();
  const model = modelIdOf(modelOf(named, DEFAULT_MODEL), 'anthropic');
  return async (ctx: restate.SqlObjectContext, message: string): Promise<string> => {
    const agent = { client, model, system, tools: tools.map((t) => (isTool(t) ? native(t, ctx) : forCall(t, ctx))) };
    const id = ctx.request().id;
    const conversation = await Conversation.open(ctx.db);
    for (;;) {
      const answer = await conversation.answer(id);
      if (answer !== undefined) return answer;
      // Finish an interrupted message first (this one's, after a crash),
      // then start this one.
      let pending = await conversation.pending();
      if (pending === undefined) {
        await conversation.append(id, [{ role: 'user', content: message }]);
        pending = id;
      }
      await run(agent, conversation, pending);
    }
  };
};

/**
 * A chat handler: each message an invocation, the conversation in the object's database.
 * It takes the message as plain text (`curl -d 'Hi'`) or as a JSON string
 * (`curl --json '"Hi"'`).
 */
export const chat = (options: ChatOptions = {}) => chatHandler(respond(options));

/** An agent: a virtual object `name`, one per key, whose `chat` handler is {@link chat}. */
export const agent = <P extends string>({
  name,
  description,
  ...options
}: ChatOptions & { name: P; description?: string }) => object({ name, description, handlers: { chat: chat(options) } });
