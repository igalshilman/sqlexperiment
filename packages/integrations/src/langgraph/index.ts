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
// back to the model until it answers. The model is `model` (a chat model, or
// `anthropic/model-id`), or else MODEL, or else anthropic/claude-sonnet-5-5,
// with ANTHROPIC_API_KEY.
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
import * as restate from '@restatedev/restate-sdk';
import type { RunnableConfig } from '@langchain/core/runnables';
import { ChatAnthropic } from '@langchain/anthropic';
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
  modelIdOf,
  modelOf,
  nextPromptId,
} from '../common.ts';
import { object } from '../object.ts';
import { isTool, type Tool as AnyTool } from '../tools.ts';

import { SqlCheckpointSaver } from './checkpointer.ts';

export { SqlCheckpointSaver };

/** The default model, of Anthropic. */
export const DEFAULT_MODEL = 'anthropic/claude-sonnet-5-5';

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
 * ToolNode would fail the attempt, and every retry the same way).
 */
function toolsNode(tools: StructuredToolInterface[]) {
  const node = new ToolNode(tools, { handleToolErrors: false });
  const names = new Set(tools.map((t) => t.name));
  return async (state: typeof MessagesAnnotation.State, config: RunnableConfig) => {
    const last = state.messages.at(-1) as AIMessage;
    const calls = last.tool_calls ?? [];
    const unknown = calls.filter((c) => !names.has(c.name));
    if (unknown.length === 0) return node.invoke(state, config);
    const known = calls.filter((c) => names.has(c.name));
    const ran: { messages: BaseMessage[] } =
      known.length === 0
        ? { messages: [] }
        : await node.invoke(
            {
              messages: [
                ...state.messages.slice(0, -1),
                new AIMessage({ id: last.id, content: last.content, tool_calls: known }),
              ],
            },
            config,
          );
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

/** The agent's graph: `model`, calling `tools`. */
function graph(model: BaseChatModel, tools: StructuredToolInterface[], system?: string) {
  if (!model.bindTools) throw new Error(`${model.getName()} does not call tools`);
  const withTools = model.bindTools(tools);
  return new StateGraph(State)
    .addNode('model', async ({ messages }) => ({
      messages: [await withTools.invoke(system ? [new SystemMessage(system), ...messages] : messages)],
    }))
    .addNode('tools', toolsNode(tools))
    .addNode('answer', ({ prompt, messages }) => ({ answers: { [prompt]: messages.at(-1)!.text } }))
    .addEdge(START, 'model')
    .addConditionalEdges('model', toolsCondition, { tools: 'tools', [END]: 'answer' })
    .addEdge('tools', 'model')
    .addEdge('answer', END);
}

/** `t` as a LangChain tool. */
const native = (t: AnyTool, ctx: restate.SqlObjectContext): StructuredToolInterface =>
  tool(
    async (input, { toolCall }) => {
      if (!toolCall?.id) throw new Error('a tool call without an id');
      return t.run(input, { ctx, callId: toolCall.id });
    },
    { name: t.name, description: t.description, schema: t.input },
  );

/** An agent's options: `chat` and `prompt` take them. */
export interface Options {
  /** The model: `anthropic/model-id` (MODEL, or else {@link DEFAULT_MODEL}), or a chat model that calls tools. */
  model?: string | BaseChatModel;
  /** The system prompt: who the agent is, and how it answers. */
  system?: string;
  /** The agent's tools: `tool()`s, Restate's (`call`, `signal`...), or LangChain's. */
  tools?: (AnyTool | StructuredToolInterface)[];
}

/**
 * Answer `text`, once: a retried attempt resumes the same prompt. Each prompt
 * of an invocation is a prompt of its own. The thread is the object's key.
 */
export const prompt = async (
  ctx: restate.SqlObjectContext,
  text: string,
  { model, system, tools = [] }: Options = {},
): Promise<string> => {
  const llm =
    typeof model === 'object'
      ? model
      : new ChatAnthropic({ model: modelIdOf(modelOf(model, DEFAULT_MODEL), 'anthropic') });
  const app = graph(
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
 * A chat handler: each message an invocation, the thread (the object's key)
 * in the object's database, with `options`, or the options a function of the
 * invocation's context gives. It takes the message as plain text
 * (`curl -d 'Hi'`) or as a JSON string (`curl --json '"Hi"'`).
 */
export const chat = (options: Configure<Options> = {}) => {
  if (typeof options === 'object' && typeof options.model === 'string') modelIdOf(options.model, 'anthropic');
  return chatHandler(async (ctx, message) => prompt(ctx, message, await configured(options, ctx)));
};

/** A handler of the thread, a page at a time: what was said, in order, with the tool calls, from its last checkpoint. */
export const messages = () =>
  messagesHandler(async (ctx, transcript) => {
    const saved = await new SqlCheckpointSaver(ctx.db).getTuple({ configurable: { thread_id: ctx.key } });
    for (const m of (saved?.checkpoint.channel_values.messages ?? []) as BaseMessage[]) {
      if (HumanMessage.isInstance(m)) transcript.say('user', m.text);
      if (AIMessage.isInstance(m)) {
        transcript.say('assistant', m.text);
        for (const call of m.tool_calls ?? []) transcript.call(call.id ?? '', call.name, call.args);
      }
      if (ToolMessage.isInstance(m)) transcript.result(m.tool_call_id, m.text, m.status === 'error');
    }
  });

/** An agent: a virtual object `name`, one per key, whose `chat` handler is {@link chat}, and `messages` {@link messages}. */
export const agent = <P extends string>({
  name,
  description,
  ...options
}: Options & { name: P; description?: string }) =>
  object({ name, description, handlers: { chat: chat(options), messages: messages() } });
