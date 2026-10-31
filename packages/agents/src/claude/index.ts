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
// - A reply cut off at `maxTokens`, or declined, ends the prompt: its tool
//   calls do not run, and the handler fails with a terminal error.
import * as restate from 'restate-objects';
import type { Database } from 'restate-objects';
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
  Transcript,
  transcribing,
  refusal,
  storedAnswer,
  UNANSWERED,
} from '../common.ts';
import { type Answer, withOutput } from '../output.ts';
import type { z } from 'zod';
import { type Definition, object } from '../object.ts';
import { isTool, jsonSchemaOf, type Tool as AnyTool } from '../tools.ts';
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
  ctx: restate.SessionContext,
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
    // A reply that cannot go on ends the prompt, whose tool calls do not run.
    const stopped = stoppedShort(message, maxTokens);
    if (stopped) {
      await conversation.close(id, stopped.text, reply);
      throw new restate.TerminalError(stopped.error);
    }
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

/**
 * Why the model's `message` stopped short of a reply to go on from: cut off
 * at `maxTokens` (its text, or a tool call's input, unfinished), or declined.
 */
function stoppedShort(message: Anthropic.Beta.Messages.BetaMessage, maxTokens: number) {
  if (message.stop_reason === 'max_tokens') {
    return {
      text: `Cut off: the reply reached ${maxTokens} tokens before it ended.`,
      error: `the model's reply reached maxTokens (${maxTokens}) before it ended: raise the agent's maxTokens`,
    };
  }
  if (message.stop_reason === 'refusal') {
    const category = message.stop_details?.category;
    return {
      text: 'Declined: the model did not answer.',
      error: `the model declined to answer${category ? ` (${category})` : ''}`,
    };
  }
  return undefined;
}

/** `t` as a tool of the Anthropic SDK's tool runner: `t.run` checks the model's arguments itself. */
const native = (t: AnyTool, ctx: restate.SessionContext): BetaRunnableTool => ({
  type: 'custom',
  name: t.name,
  description: t.description,
  input_schema: jsonSchemaOf(t) as Anthropic.Beta.Messages.BetaTool.InputSchema,
  parse: (args) => args,
  run: (args, context) => t.run(args, { ctx, callId: callId(context) }),
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
  /**
   * The most tokens of each of the model's replies (default 16000). A reply
   * cut off there ends its prompt, which fails (a terminal error).
   */
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
const promptText = (ctx: restate.SessionContext, text: string, options: Options): Promise<string> =>
  transcribing(
    ctx,
    () => transcribe(ctx.db),
    () => answerOnce(ctx, text, options),
  );

const answerOnce = async (
  ctx: restate.SessionContext,
  text: string,
  { model: named, system, tools = [], client, maxTokens = 16_000, effort }: Options,
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
    if (answer !== undefined) return storedAnswer(answer);
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
      // Refused for good (a wrong API key): the prompt ends unanswered, and
      // fails as terminal. An earlier one's, this one goes on.
      const refused = refusal(error);
      if (!refused) throw error;
      await conversation.close(pending, UNANSWERED);
      if (pending === id) throw refused;
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
  ctx: restate.SessionContext,
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

/**
 * The transcript of each session's conversation (`ctx.db`, one per
 * attempt), as far as it was read: the conversation only grows, so each
 * read adds the messages stored since.
 */
const transcripts = new WeakMap<Database, { seq: number; transcript: Transcript }>();

/** The conversation in `db`, as `messages` gives it. */
const transcribe = async (db: Database): Promise<Transcript> => {
  const read = transcripts.get(db) ?? { seq: 0, transcript: new Transcript() };
  transcripts.set(db, read);
  const { transcript } = read;
  for (const [seq, m] of await (await Conversation.open(db)).since(read.seq)) {
    read.seq = seq;
    if (m.role === 'system') continue;
    transcript.say(m.role, storedText(m.content));
    if (typeof m.content === 'string') continue;
    for (const block of m.content) {
      if (block.type === 'tool_use') transcript.call(block.id, block.name, block.input);
      if (block.type === 'tool_result')
        transcript.result(block.tool_use_id, storedText(block.content), block.is_error === true);
    }
  }
  return transcript;
};

/** An agent: a virtual object `name`, one per key, whose `chat` handler is {@link chat}, and `messages` {@link messages}. */
export const agent = <P extends string, O extends z.ZodObject | undefined = undefined>({
  name,
  description,
  schema,
  background,
  ...options
}: Options<O> & Definition<P>) =>
  object({ name, description, schema, background, handlers: { chat: chat(options) }, views: { messages: messages() } });
