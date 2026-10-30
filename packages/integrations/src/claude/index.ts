// Claude agents in Restate virtual objects, on the tool runner of the
// Anthropic SDK (`client.beta.messages.toolRunner`): the conversation is in
// the object's database (conversation.ts).
//
//   const assistant = claude.agent({ name: 'assistant', system: 'Answer briefly.', tools: [weather] });
//
// or a handler of an object of your own: `handlers: { chat: claude.chat() }`,
// with options for each invocation: `claude.chat(async (ctx) => ({ system }))`,
// or a prompt of your own handler: `claude.prompt(ctx, text, options)`
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
import * as restate from 'restate-fns';
import type { SqlSession } from 'restate-fns';
import Anthropic from '@anthropic-ai/sdk';
import type { BetaRunnableTool, BetaToolRunContext } from '@anthropic-ai/sdk/lib/tools/BetaRunnableTool';
import {
  aborted,
  CANCELLED,
  cancellationOf,
  chatHandler,
  CLAUDE_MODEL,
  type Configure,
  configured,
  messagesHandler,
  modelIdOf,
  modelOf,
  nextPromptId,
  toolCallEnded,
  toolCallStarted,
  type Transcript,
  transcribing,
} from '../common.ts';
import { type Answer, withOutput } from '../output.ts';
import type { z } from 'zod';
import { type Definition, object } from '../object.ts';
import { betaZodTool } from '@anthropic-ai/sdk/helpers/beta/zod';
import { isTool, type Tool as AnyTool } from '../tools.ts';
import { Conversation } from './conversation.ts';

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
  maxTokens: number;
  effort?: Effort;
  system?: string;
  tools: BetaRunnableTool[];
}

/**
 * Run prompt `id` of `conversation` until it is answered, from its stored
 * messages; each round of tool calls published as it starts and ends (for
 * the clients that follow the conversation).
 */
async function run(
  ctx: restate.ObjectContext,
  { client, model, maxTokens, effort, system, tools: all }: Agent,
  conversation: Conversation,
  id: string,
  signal?: AbortSignal,
) {
  const { tools, check } = failing(all);
  const runner = client.beta.messages.toolRunner(
    {
      model,
      max_tokens: maxTokens,
      ...(effort === undefined ? {} : { output_config: { effort } }),
      ...(system === undefined ? {} : { system }),
      tools,
      messages: await conversation.messages(),
    },
    { signal },
  );
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
    await toolCallStarted(ctx);
    const results = await runner.generateToolResponse();
    check();
    if (results) await conversation.append(id, [results]);
    await toolCallEnded(ctx);
  }
}

/** `t` as a tool of the Anthropic SDK's tool runner. */
const native = (t: AnyTool, ctx: restate.ObjectContext): BetaRunnableTool =>
  betaZodTool({
    name: t.name,
    description: t.description,
    inputSchema: t.input,
    run: (input, context) => t.run(input, { ctx, callId: callId(context) }),
  });

/** How much effort the model puts into a reply (Anthropic's `output_config.effort`). */
export type Effort = 'low' | 'medium' | 'high' | 'xhigh' | 'max';

/** An agent's options: `chat` and `prompt` take them. */
export interface Options<O extends z.ZodObject | undefined = z.ZodObject | undefined> {
  /** The model, as `anthropic/model-id`: MODEL, or else anthropic/claude-sonnet-5-5. */
  model?: string;
  /** The system prompt. */
  system?: string;
  /** The agent's tools: `tool()`s, Restate's (`call`, `signal`...), or the SDK's (such as `betaZodTool`s). */
  tools?: (AnyTool | BetaRunnableTool)[];
  /** The client of the Anthropic API: by default one with ANTHROPIC_API_KEY. */
  client?: Anthropic;
  /** The most tokens of each of the model's replies (default 4096). */
  maxTokens?: number;
  /** How much effort the model puts into each reply: more is more thorough, and slower (default: the model's). */
  effort?: Effort;
  /**
   * The answer as data: a zod object, whose JSON the agent replies with
   * (checked), and `chat` and `prompt` answer with (see output.ts).
   */
  output?: O;
}

/** The client of the agents without one of their own, made when first used. */
let shared: Anthropic | undefined;

/** {@link prompt}, answering with the agent's text. */
const promptText = (ctx: restate.ObjectContext, text: string, options: Options): Promise<string> =>
  transcribing(
    ctx,
    (transcript) => transcribe(ctx.db, transcript),
    () => answerOnce(ctx, text, options),
  );

const answerOnce = async (
  ctx: restate.ObjectContext,
  text: string,
  { model: named, system, tools = [], client, maxTokens = 4096, effort }: Options,
): Promise<string> => {
  const agent = {
    client: client ?? (shared ??= new Anthropic()),
    model: modelIdOf(modelOf(named, CLAUDE_MODEL), 'anthropic'),
    maxTokens,
    effort,
    system,
    tools: tools.map((t) => (isTool(t) ? native(t, ctx) : t)),
  };
  const id = nextPromptId(ctx);
  const conversation = await Conversation.open(ctx.db);
  for (;;) {
    const answer = await conversation.answer(id);
    if (answer !== undefined) return answer;
    // Finish an interrupted prompt first (this one's, after a crash), then
    // start this one.
    let pending = await conversation.pending();
    if (pending === undefined) {
      await conversation.append(id, [{ role: 'user', content: text }]);
      pending = id;
    }
    const turn = cancellationOf(ctx);
    try {
      const running = run(ctx, agent, conversation, pending, turn);
      running.catch(() => {});
      await (turn ? Promise.race([running, aborted(turn)]) : running);
    } catch (error) {
      // Cancelled: the prompt ends, and the next one starts afresh.
      if (turn?.aborted || error instanceof restate.CancelledError) {
        await conversation.cancel(pending, CANCELLED);
        throw turn?.aborted ? turn.reason : error;
      }
      throw error;
    }
  }
};

/**
 * Answer `text`, once: a retried attempt resumes the same prompt. Each prompt
 * of an invocation is a prompt of its own.
 * With `output`, it answers with the value the agent replies with (see
 * output.ts), not its text.
 */
export const prompt = <O extends z.ZodObject | undefined = undefined>(
  ctx: restate.ObjectContext,
  text: string,
  options: Options<O> = {},
): Promise<Answer<O>> => withOutput(text, options, (t, o) => promptText(ctx, t, o)) as Promise<Answer<O>>;

/**
 * A chat handler: each message an invocation, the conversation in the
 * object's database, with `options`, or the options a function of the
 * invocation's context gives. It takes the message as plain text
 * (`curl -d 'Hi'`) or as a JSON string (`curl --json '"Hi"'`).
 */
export const chat = <O extends z.ZodObject | undefined = undefined>(options: Configure<Options<O>> = {}) => {
  if (typeof options === 'object') modelIdOf(modelOf(options.model, CLAUDE_MODEL), 'anthropic');
  return chatHandler(async (ctx, message) => prompt(ctx, message, await configured(options, ctx)));
};

/** The text of stored content, without its tool calls and results. */
const storedText = (content: string | readonly { type: string; text?: string }[] | undefined) =>
  typeof content === 'string' ? content : (content ?? []).flatMap((b) => (b.type === 'text' ? [b.text] : [])).join('');

/**
 * A handler of the conversation as clients read it: a page at a time, or
 * what is new or changed since a cursor, from the log each change writes
 * (log.ts). It answers while a message is answered.
 */
export const messages = () => messagesHandler();

/** The conversation in `db`, as `messages` gives it. */
const transcribe = async (db: SqlSession, transcript: Transcript) => {
  const conversation = await Conversation.open(db);
  for (const m of await conversation.messages()) {
    if (m.role === 'system') continue;
    transcript.say(m.role, storedText(m.content));
    if (typeof m.content === 'string') continue;
    for (const block of m.content) {
      if (block.type === 'tool_use') transcript.call(block.id, block.name, block.input);
      if (block.type === 'tool_result')
        transcript.result(block.tool_use_id, storedText(block.content), block.is_error === true);
    }
  }
};

/** An agent: a virtual object `name`, one per key, whose `chat` handler is {@link chat}, and `messages` {@link messages}. */
export const agent = <P extends string, O extends z.ZodObject | undefined = undefined>({
  name,
  description,
  schema,
  background,
  ...options
}: Options<O> & Definition<P>) =>
  object({ name, description, schema, background, handlers: { chat: chat(options), messages: messages() } });
