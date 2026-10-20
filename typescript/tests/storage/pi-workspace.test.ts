// withPi over a sqlite-kv database in memory, without a Restate server: the
// coding tools of a workspace write to the local file system.
import type * as restate from 'restate-sql';
import { mkdtemp, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createModels } from '@earendil-works/pi-ai/models';
import { open } from 'sqlite-kv';
import { describe, expect, it } from 'vitest';
import { memoryStore } from '../testing.ts';
import { callTools, fakeProvider, say } from '../fakes/pi.ts';
import { withPi } from 'restate-agents/pi';

describe('withPi', () => {
  it('gives a workspace the coding tools, over the local file system', async () => {
    const root = await mkdtemp(join(tmpdir(), 'with-pi-'));
    const workspace = join(root, 'Assistant', 'acme.alice');
    const db = await open(`with-pi-${process.pid}`, memoryStore());
    const models = createModels();
    models.setProvider(
      fakeProvider('scribe', 'scribe', (context) => {
        const last = context.messages.at(-1)!;
        if (last.role === 'toolResult') return say(last.isError ? 'failed' : 'written');
        return callTools([['write', { path: 'notes.md', content: 'crushed' }]]);
      }),
    );
    try {
      const ctx = { db } as unknown as restate.ObjectContext;
      const { answer, agent } = await withPi(ctx, { models, model: 'scribe/scribe', workspace }, async (pi) => ({
        answer: await pi.prompt('take a note', 'p1'),
        agent: await (await pi.harness.root(pi.context)).agent(pi.context),
      }));
      expect(answer).toBe('written');
      expect(agent.tools.map((t) => t.name)).toEqual(['read', 'write', 'edit', 'bash']);
      expect(agent.sections.map((s) => s.key)).toContain('cwd');
      expect(await readFile(join(workspace, 'notes.md'), 'utf8')).toBe('crushed');
    } finally {
      await db.close();
      await rm(root, { recursive: true, force: true });
    }
  });
});
