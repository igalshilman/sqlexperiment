// A thread checkpointed by the earlier checkpointer.ts, whose versions were
// numbers, goes on with the current one, whose versions are strings.
// `pnpm --filter restate-agents test langgraph`.
import { Annotation, END, START, StateGraph } from '@langchain/langgraph';
import { open } from 'restate-sql';
import { describe, expect, it } from 'vitest';
import { memoryStore } from 'restate-objects/test/support';
import { SqlCheckpointSaver } from 'restate-agents/langgraph';

const State = Annotation.Root({
  count: Annotation<number>({ reducer: (a, b) => a + b, default: () => 0 }),
  text: Annotation<string>,
});

/** The earlier saver: LangGraph's default versions, numbers. */
class NumberedSaver extends SqlCheckpointSaver {
  override getNextVersion(current: number | undefined): number {
    return (current ?? 0) + 1;
  }
}

const graph = (saver: SqlCheckpointSaver) =>
  new StateGraph(State)
    .addNode('step', (state) => ({ count: 1, text: `${state.text}.` }))
    .addEdge(START, 'step')
    .addEdge('step', END)
    .compile({ checkpointer: saver });

describe('a thread of the earlier checkpointer', () => {
  it('goes on with its values, and its steps still run', async () => {
    const db = await open(`langgraph-upgrade-${process.pid}`, memoryStore());
    const thread = { configurable: { thread_id: 't' } };

    const before = graph(new NumberedSaver(db));
    await before.invoke({ text: 'a' }, thread);
    await before.invoke({ text: 'b' }, thread);
    expect((await before.getState(thread)).values).toEqual({ count: 2, text: 'b.' });

    const after = graph(new SqlCheckpointSaver(db));
    expect((await after.getState(thread)).values).toEqual({ count: 2, text: 'b.' });
    await after.invoke({ text: 'c' }, thread);
    expect((await after.getState(thread)).values).toEqual({ count: 3, text: 'c.' });
    await after.invoke({ text: 'd' }, thread);
    expect((await after.getState(thread)).values).toEqual({ count: 4, text: 'd.' });
    await db.close();
  });
});
