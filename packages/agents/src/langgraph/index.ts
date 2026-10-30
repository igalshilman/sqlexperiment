// LangGraph agents in Restate virtual objects: a tool-calling graph whose
// checkpoints are in the object's database (checkpointer.ts).
//
//   const assistant = langgraph.agent({ name: 'assistant', tools: [search] });
//
// or a handler of an object of your own: `handlers: { chat: langgraph.chat() }`,
// with options for each invocation: `langgraph.chat(async (ctx) => ({ system }))`,
// or a prompt of your own handler: `langgraph.prompt(ctx, text, options)`
//
// The graph is the usual tool-calling loop: a model node, a tools node, and
// back to the model until it answers. The model is `model` (a chat model,
// `openai/model-id` or `anthropic/model-id`), or else MODEL, or else
// openai/gpt-6-sol, with OPENAI_API_KEY.
//
// The handler is not replayed: a retried attempt (a crash, a lost connection)
// runs again from the start, and LangGraph carries on from the last step it
// stored:
// - Each step is stored before the next starts (`durability: 'sync'`), in
//   order with everything else the handler does. So the tool calls a model
//   made are stored before any tool runs, and a crash during the tools
//   resumes them with the same tool call ids, which a tool passes on to stay
//   idempotent.
// - A tool's error fails the attempt, which Restate retries (the ToolNode
//   would hand it to the model as the tool's result).
// - A message is answered once per invocation: the graph records each answer
//   in its state, and a retried attempt returns it.
import * as restate from 'restate-objects';
import type { SqlSession } from 'restate-objects';
import type { RunnableConfig } from '@langchain/core/runnables';
import { ChatAnthropic } from '@langchain/anthropic';
import { ChatOpenAI } from '@langchain/openai';
import type { BaseChatModel } from '@langchain/core/language_models/chat_models';
import { AIMessage, type BaseMessage, HumanMessage, SystemMessage, ToolMessage } from '@langchain/core/messages';
import { type StructuredToolInterface, tool } from '@langchain/core/tools';
import { Annotation, END, MessagesAnnotation, START, StateGraph } from '@langchain/langgraph';
import { ToolNode, toolsCondition } from '@langchain/langgraph/prebuilt';
import {
  CANCELLED,
  cancellationOf,
  chatHandler,
  type Configure,
  configured,
  messagesHandler,
  ModelError,
  modelOf,
  nextPromptId,
  providerOf,
  toolCallEnded,
  toolCallStarted,
  type Transcript,
  transcribing,
} from '../common.ts';
import { type Answer, withOutput } from '../output.ts';
import type { z } from 'zod';
import { type Definition, object } from '../object.ts';
import { isTool, type Tool as AnyTool } from '../tools.ts';

import { SqlCheckpointSaver } from './checkpointer.ts';

export { SqlCheckpointSaver };

const State = Annotation.Root({
  ...MessagesAnnotation.spec,
  /** The id of the prompt being answered. */
  prompt: Annotation<string>(),
  /** The answer to each prompt, by id. */
  answers: Annotation<Record<string, string>>({ reducer: (a, b) => ({ ...a, ...b }), default: () => ({}) }),
});

/**
 * The tools node: each tool call of the model's last message run by a
 * ToolNode, whose tool's error fails the attempt; and a call of a tool the
 * agent does not have answered with an error, for the model to see (the
 * ToolNode would fail the attempt, and every retry the same way). The round
 * is published as it starts and ends, for the clients that follow the
 * conversation.
 */
function toolsNode(ctx: restate.ObjectContext, tools: StructuredToolInterface[]) {
  const run = runTools(tools);
  return async (state: typeof MessagesAnnotation.State, config: RunnableConfig) => {
    await toolCallStarted(ctx);
    const ran = await run(state, config);
    await toolCallEnded(ctx);
    return ran;
  };
}

/** The tool calls of the model's last message: the tools the agent has, run by a ToolNode, and an error for the others. */
function runTools(tools: StructuredToolInterface[]) {
  const node = new ToolNode(tools, { handleToolErrors: false });
  const names = new Set(tools.map((t) => t.name));
  return async (
    state: typeof MessagesAnnotation.State,
    config: RunnableConfig,
  ): Promise<{ messages: BaseMessage[] }> => {
    const last = state.messages.at(-1) as AIMessage;
    const calls = last.tool_calls ?? [];
    const unknown = calls.filter((c) => !names.has(c.name));
    if (unknown.length === 0) return node.invoke(state, config);
    // The calls of tools the agent has, run as the model's last message with those calls only.
    const known = calls.filter((c) => names.has(c.name));
    const only = new AIMessage({ id: last.id, content: last.content, tool_calls: known });
    const ran: { messages: BaseMessage[] } =
      known.length > 0
        ? await node.invoke({ messages: [...state.messages.slice(0, -1), only] }, config)
        : { messages: [] };
    const missing = unknown.map(
      (c) =>
        new ToolMessage({
          tool_call_id: c.id!,
          name: c.name,
          status: 'error',
          content: `Error: Tool "${c.name}" not found.`,
        }),
    );
    return { messages: [...ran.messages, ...missing] };
  };
}

/** `provider/model-id` as its provider and model id, for a provider the usual graph takes. */
const providerOfModel = (named: string) => {
  const [provider, id] = providerOf(named);
  if ((provider !== 'openai' && provider !== 'anthropic') || !id) {
    const from = process.env.MODEL === named ? ' (from MODEL)' : '';
    throw new ModelError(`the model "${named}"${from} is neither openai/<model-id> nor anthropic/<model-id>`);
  }
  return [provider, id] as const;
};

/** The chat model `provider/model-id` names: OpenAI's or Anthropic's. */
const chatModel = (named: string): BaseChatModel => {
  const [provider, id] = providerOfModel(named);
  return provider === 'openai' ? new ChatOpenAI({ model: id }) : new ChatAnthropic({ model: id });
};

/** The agent's graph: `model`, calling `tools`. */
function graph(ctx: restate.ObjectContext, model: BaseChatModel, tools: StructuredToolInterface[], system?: string) {
  if (!model.bindTools) throw new Error(`${model.getName()} does not call tools`);
  const withTools = model.bindTools(tools);
  return new StateGraph(State)
    .addNode('model', async ({ messages }) => ({
      messages: [await withTools.invoke(system ? [new SystemMessage(system), ...messages] : messages)],
    }))
    .addNode('tools', toolsNode(ctx, tools))
    .addNode('answer', ({ prompt, messages }) => ({ answers: { [prompt]: messages.at(-1)!.text } }))
    .addEdge(START, 'model')
    .addConditionalEdges('model', toolsCondition, { tools: 'tools', [END]: 'answer' })
    .addEdge('tools', 'model')
    .addEdge('answer', END);
}

/** `t` as a LangChain tool. */
const native = (t: AnyTool, ctx: restate.ObjectContext): StructuredToolInterface =>
  tool(
    async (input, { toolCall }) => {
      if (!toolCall?.id) throw new Error('a tool call without an id');
      return t.run(input, { ctx, callId: toolCall.id });
    },
    { name: t.name, description: t.description, schema: t.input },
  );

/** An agent's options: `chat` and `prompt` take them. */
export interface Options<O extends z.ZodObject | undefined = z.ZodObject | undefined> {
  /** The model: `openai/model-id` or `anthropic/model-id` (MODEL, or else openai/gpt-6-sol), or a chat model that calls tools. */
  model?: string | BaseChatModel;
  /** The system prompt: who the agent is, and how it answers. */
  system?: string;
  /** The agent's tools: `tool()`s, Restate's (`call`, `signal`...), or LangChain's. */
  tools?: (AnyTool | StructuredToolInterface)[];
  /**
   * The answer as data: a zod object, whose JSON the agent replies with
   * (checked), and `chat` and `prompt` answer with (see output.ts).
   */
  output?: O;
}

/** {@link prompt}, answering with the agent's text. */
const promptText = (ctx: restate.ObjectContext, text: string, options: Options): Promise<string> =>
  transcribing(
    ctx,
    (transcript) => transcribe(ctx.db, ctx.key, transcript),
    () => answerOnce(ctx, text, options),
  );

const answerOnce = async (
  ctx: restate.ObjectContext,
  text: string,
  { model, system, tools = [] }: Options,
): Promise<string> => {
  const llm = typeof model === 'object' ? model : chatModel(modelOf(model));
  const app = graph(
    ctx,
    llm,
    tools.map((t) => (isTool(t) ? native(t, ctx) : t)),
    system,
  ).compile({ checkpointer: new SqlCheckpointSaver(ctx.db) });
  const config = { configurable: { thread_id: ctx.key }, durability: 'sync' as const };
  const id = nextPromptId(ctx);
  for (;;) {
    const state = await app.getState(config);
    const answer = state.values.answers?.[id];
    if (answer !== undefined) return answer;
    // Finish an interrupted run first (this prompt's, after a crash), then
    // start this prompt's. A run is unfinished while its prompt has no
    // answer: `next` does not say, as it leaves out the steps whose writes
    // are stored.
    const { prompt: last, answers = {} } = state.values;
    const interrupted = last !== undefined && answers[last] === undefined;
    const turn = cancellationOf(ctx);
    try {
      await app.invoke(interrupted ? null : { messages: [new HumanMessage(text)], prompt: id }, {
        ...config,
        signal: turn,
      });
    } catch (error) {
      // Cancelled: the run ends, and the next prompt starts afresh.
      if (turn?.aborted || error instanceof restate.CancelledError) {
        await cancel(app, config);
        throw turn?.aborted ? turn.reason : error;
      }
      throw error;
    }
  }
};

/**
 * End the run of `app` that was cancelled, as its `answer` node: a result for
 * each tool call of the last reply that has none, then {@link CANCELLED} as
 * the reply and the prompt's answer.
 */
async function cancel(app: ReturnType<ReturnType<typeof graph>['compile']>, config: RunnableConfig) {
  const { values } = await app.getState(config);
  const messages = (values.messages ?? []) as BaseMessage[];
  const answered = new Set(messages.filter((m) => ToolMessage.isInstance(m)).map((m) => m.tool_call_id));
  const last = messages.findLast((m) => AIMessage.isInstance(m));
  const calls = (last && AIMessage.isInstance(last) ? (last.tool_calls ?? []) : []).filter(
    (call) => call.id !== undefined && !answered.has(call.id),
  );
  await app.updateState(
    config,
    {
      messages: [
        ...calls.map((call) => new ToolMessage({ tool_call_id: call.id!, content: CANCELLED, status: 'error' })),
        new AIMessage(CANCELLED),
      ],
      answers: { [values.prompt]: CANCELLED },
    },
    'answer',
  );
}

/**
 * Answer `text`, once: a retried attempt resumes the same prompt. Each prompt
 * of an invocation is a prompt of its own. The thread is the object's key.
 * With `output`, it answers with the value the agent replies with (see
 * output.ts), not its text.
 */
export const prompt = <O extends z.ZodObject | undefined = undefined>(
  ctx: restate.ObjectContext,
  text: string,
  options: Options<O> = {},
): Promise<Answer<O>> => withOutput(text, options, (t, o) => promptText(ctx, t, o)) as Promise<Answer<O>>;

/**
 * A chat handler: each message an invocation, the thread (the object's key)
 * in the object's database, with `options`, or the options a function of the
 * invocation's context gives. It takes the message as plain text
 * (`curl -d 'Hi'`) or as a JSON string (`curl --json '"Hi"'`).
 */
export const chat = <O extends z.ZodObject | undefined = undefined>(options: Configure<Options<O>> = {}) => {
  if (typeof options === 'object' && typeof options.model !== 'object') providerOfModel(modelOf(options.model));
  return chatHandler(async (ctx, message) => prompt(ctx, message, await configured(options, ctx)));
};

/**
 * A handler of the conversation as clients read it: a page at a time, or
 * what is new or changed since a cursor, from the log each change writes
 * (log.ts). It answers while a message is answered.
 */
export const messages = () => messagesHandler();

/** The thread `key` in `db`, from its last checkpoint, as `messages` gives it. */
const transcribe = async (db: SqlSession, key: string, transcript: Transcript) => {
  const saved = await new SqlCheckpointSaver(db).getTuple({ configurable: { thread_id: key } });
  for (const m of (saved?.checkpoint.channel_values.messages ?? []) as BaseMessage[]) {
    if (HumanMessage.isInstance(m)) transcript.say('user', m.text);
    if (AIMessage.isInstance(m)) {
      transcript.say('assistant', m.text);
      for (const call of m.tool_calls ?? []) transcript.call(call.id ?? '', call.name, call.args);
    }
    if (ToolMessage.isInstance(m)) transcript.result(m.tool_call_id, m.text, m.status === 'error');
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
