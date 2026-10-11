// A LangGraph agent in a Restate virtual object: a GraphAgent object per
// conversation (the LangGraph thread), whose checkpoints are in the object's
// database (checkpointer.mjs).
//
// The graph is the usual tool-calling loop: a model node, a tools node, and
// back to the model until it answers. Its tools move money in the ledger
// (../node/ledger.mjs).
//
// The handler is not replayed: a retried attempt (a crash, a lost connection)
// runs again from the start, and LangGraph carries on from the last step it
// stored:
// - Each step is stored before the next starts (`durability: 'sync'`), in
//   order with everything else the handler does. So the tool calls a model
//   made are stored before any tool runs, and a crash during the tools
//   resumes them with the same tool call ids.
// - A tool uses its tool call id as the transfer id, so a tool run again moves
//   the money once; and its call to the ledger has an idempotency key made
//   from the request id (`ctx.request().id`, the same on every attempt) and
//   the tool call id, so a retried attempt reaches the call the first one made.
// - A prompt is answered once per prompt id: the graph records each answer in
//   its state, and a prompt asked again returns it.
//
// Environment: PORT (default 9080) and ABORT_PER_MILLE (see ../faults.mjs).
import * as restate from '@restatedev/restate-sdk';
import { HumanMessage } from '@langchain/core/messages';
import { tool } from '@langchain/core/tools';
import { Annotation, END, MessagesAnnotation, START, StateGraph } from '@langchain/langgraph';
import { ToolNode, toolsCondition } from '@langchain/langgraph/prebuilt';
import { z } from 'zod';
import { serveIfMain } from '../faults.mjs';
import { ledger } from '../node/ledger.mjs';
import { SqlCheckpointSaver } from './checkpointer.mjs';
import { Teller } from './teller.mjs';

/** The bank tools of one invocation: they reach the ledger through `ctx`. */
const bank = (ctx) => {
  const books = ctx.objectClient(ledger, 'bank');
  // One idempotency key per tool call: a prompt makes several.
  const opts = (toolCall) => restate.rpc.opts({ idempotencyKey: `${ctx.request().id}:${toolCall.id}` });
  return [
    tool(
      async ({ from, to, amount }, { toolCall }) => {
        const id = `GraphAgent/${ctx.key}/${toolCall.id}`; // unique in the ledger
        await books.transfer({ id, from, to, amount }, opts(toolCall));
        return `moved ${amount} from account ${from} to account ${to}`;
      },
      {
        name: 'transfer',
        description: 'Move an amount of money from one account to another.',
        schema: z.object({
          from: z.number().int().describe('Account to take the money from'),
          to: z.number().int().describe('Account to put the money in'),
          amount: z.number().int().min(1),
        }),
      },
    ),
    tool(async (_args, { toolCall }) => JSON.stringify(await books.audit(opts(toolCall))), {
      name: 'audit',
      description: 'Check the books: accounts, total money, transfers, and inconsistent accounts.',
      schema: z.object({}),
    }),
  ];
};

const State = Annotation.Root({
  ...MessagesAnnotation.spec,
  /** The id of the prompt being answered. */
  prompt: Annotation(),
  /** The answer to each prompt, by id. */
  answers: Annotation({ reducer: (a, b) => ({ ...a, ...b }), default: () => ({}) }),
});

/** The agent's graph, with the tools of this invocation. */
function graph(ctx, model) {
  const tools = bank(ctx);
  const withTools = model.bindTools(tools);
  return new StateGraph(State)
    .addNode('model', async ({ messages }) => ({ messages: [await withTools.invoke(messages)] }))
    // A tool's error fails the attempt, which Restate retries (by default the
    // ToolNode would hand it to the model as the tool's result).
    .addNode('tools', new ToolNode(tools, { handleToolErrors: false }))
    .addNode('answer', ({ prompt, messages }) => ({ answers: { [prompt]: messages.at(-1).text } }))
    .addEdge(START, 'model')
    .addConditionalEdges('model', toolsCondition, { tools: 'tools', [END]: 'answer' })
    .addEdge('tools', 'model')
    .addEdge('answer', END);
}

const model = new Teller({});

export const agent = restate.object({
  name: 'GraphAgent',
  handlers: {
    /** Answer prompt `text`, once per prompt `id`. */
    prompt: (ctx, { id, text }) =>
      ctx.sqlSession(async (db) => {
        const app = graph(ctx, model).compile({ checkpointer: new SqlCheckpointSaver(db) });
        const config = { configurable: { thread_id: ctx.key }, durability: 'sync' };
        for (;;) {
          const state = await app.getState(config);
          const answer = state.values.answers?.[id];
          if (answer !== undefined) return { answer };
          // Finish an interrupted run first (this prompt's, after a crash),
          // then start this prompt's.
          await app.invoke(
            state.next.length > 0 ? null : { messages: [new HumanMessage(text)], prompt: id },
            config,
          );
        }
      }),
  },
  options: {
    ...restate.sqlOptions,
    // Keep retrying rather than pause after Restate's default number of
    // attempts: each attempt resumes from the last stored step.
    retryPolicy: { initialInterval: 50, maxInterval: 500, maxAttempts: 100_000 },
  },
});

serveIfMain(import.meta, [agent, ledger]);
