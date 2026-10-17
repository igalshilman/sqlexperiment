// LangGraph agents in Restate virtual objects: a tool-calling graph whose
// checkpoints are in the object's database (checkpointer.ts).
//
//   const assistant = langgraph.agent({ name: 'assistant', tools: [search] });
//
// or a handler of an object of your own: `handlers: { chat: langgraph.chat() }`
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
import type * as restate from '@restatedev/restate-sdk';
import { ChatAnthropic } from '@langchain/anthropic';
import type { BaseChatModel } from '@langchain/core/language_models/chat_models';
import { HumanMessage } from '@langchain/core/messages';
import { type StructuredToolInterface, tool } from '@langchain/core/tools';
import { Annotation, END, MessagesAnnotation, START, StateGraph } from '@langchain/langgraph';
import { ToolNode, toolsCondition } from '@langchain/langgraph/prebuilt';
import { chatHandler, forCall, modelIdOf, modelOf, type PerCall } from '../common.ts';
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

/** The agent's graph: `model`, calling `tools`. */
function graph(model: BaseChatModel, tools: StructuredToolInterface[]) {
  if (!model.bindTools) throw new Error(`${model.getName()} does not call tools`);
  const withTools = model.bindTools(tools);
  return new StateGraph(State)
    .addNode('model', async ({ messages }) => ({ messages: [await withTools.invoke(messages)] }))
    .addNode('tools', new ToolNode(tools, { handleToolErrors: false }))
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

export interface ChatOptions {
  /**
   * The model: `anthropic/model-id` (MODEL, or else {@link DEFAULT_MODEL}),
   * or a chat model that calls tools, or for an invocation.
   */
  model?: PerCall<string | BaseChatModel>;
  /** The agent's tools: `tool()`s, Restate's (`call`, `signal`...), or LangChain's, or for an invocation. */
  tools?: (PerCall<StructuredToolInterface> | AnyTool)[];
}

/** The answer to a message, each message an invocation, the thread (the object's key) in the object's database. */
const respond =
  ({ model, tools = [] }: ChatOptions = {}) =>
  async (ctx: restate.SqlObjectContext, message: string): Promise<string> => {
    const chosen = model === undefined ? undefined : forCall(model, ctx);
    const llm =
      typeof chosen === 'object'
        ? chosen
        : new ChatAnthropic({ model: modelIdOf(modelOf(chosen, DEFAULT_MODEL), 'anthropic') });
    const app = graph(
      llm,
      tools.map((t) => (isTool(t) ? native(t, ctx) : forCall(t, ctx))),
    ).compile({ checkpointer: new SqlCheckpointSaver(ctx.db) });
    const config = { configurable: { thread_id: ctx.key }, durability: 'sync' as const };
    const id = ctx.request().id;
    for (;;) {
      const state = await app.getState(config);
      const answer = state.values.answers?.[id];
      if (answer !== undefined) return answer;
      // Finish an interrupted run first (this message's, after a crash),
      // then start this message's.
      await app.invoke(state.next.length > 0 ? null : { messages: [new HumanMessage(message)], prompt: id }, config);
    }
  };

/**
 * A chat handler: each message an invocation, the thread (the object's key) in the object's database.
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
