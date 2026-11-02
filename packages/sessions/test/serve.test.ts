// What `restate.serve` serves: the services given, each with what it needs
// beside it (a library's own objects), once, or those RESTATE_SERVE_ONLY names.
import * as restate from 'restate-sessions';
import { needing, served } from 'restate-sessions/internal';
import { describe, expect, it } from 'vitest';

const helper = restate.service({ name: 'helper', handlers: { help: async (_ctx: restate.Context) => 'ok' } });
const notes = needing(restate.sessionObject({ name: 'notes', handlers: { add: async () => {} } }), () => [helper]);
const lists = needing(restate.sessionObject({ name: 'lists', handlers: { add: async () => {} } }), () => [helper]);

const names = (services: { name: string }[]) => services.map((s) => s.name);

describe('restate.serve', () => {
  it('serves each service with what it needs beside it, once', () => {
    expect(names(served([notes, lists]))).toEqual(['notes', 'helper', 'lists']);
    expect(names(served([helper, notes]))).toEqual(['helper', 'notes']);
  });

  it('serves only the services RESTATE_SERVE_ONLY names, and refuses a name of none', () => {
    expect(names(served([notes, lists], 'lists, helper'))).toEqual(['helper', 'lists']);
    expect(() => served([notes], 'notes,nobody')).toThrow(/RESTATE_SERVE_ONLY names nobody/);
  });
});
