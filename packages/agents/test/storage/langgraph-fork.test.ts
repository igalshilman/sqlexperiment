// A thread forked from an earlier checkpoint, on checkpointer.ts: each branch
// keeps its own channel values. `pnpm --filter restate-agents test langgraph`.
import { Annotation, END, START, StateGraph } from '@langchain/langgraph';
import { open } from 'restate-sql';
import { describe, expect, it } from 'vitest';
import { memoryStore } from 'restate-objects/test/support';
import { SqlCheckpointSaver } from 'restate-agents/langgraph';

const State = Annotation.Root({ text: Annotation<string> });

describe('a forked thread', () => {
  it('keeps the values of the branch it forked from', async () => {
    const db = await open(`langgraph-fork-${process.pid}`, memoryStore());
    const graph = new StateGraph(State)
      .addNode('shout', (state) => ({ text: `${state.text}!` }))
      .addEdge(START, 'shout')
      .addEdge('shout', END)
      .compile({ checkpointer: new SqlCheckpointSaver(db) });
    const thread = { configurable: { thread_id: 't' } };

    await graph.invoke({ text: 'a' }, thread);
    const original = await graph.getState(thread);
    expect(original.values).toEqual({ text: 'a!' });

    // Fork from the checkpoint before the step, with another value.
    const history = [];
    for await (const state of graph.getStateHistory(thread)) history.push(state);
    const before = history.find((state) => state.next.includes('shout'))!;
    const forked = await graph.updateState(before.config, { text: 'b' });
    await graph.invoke(null, forked);
    expect((await graph.getState(thread)).values).toEqual({ text: 'b!' });

    // The original branch reads as it was.
    expect((await graph.getState(original.config)).values).toEqual({ text: 'a!' });
    expect((await graph.getState(before.config)).values).toEqual({ text: 'a' });
    await db.close();
  });
});
