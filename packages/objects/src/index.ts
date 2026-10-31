// Session objects: Restate virtual objects whose state is a SQL database per
// key. It is the Restate SDK, with `restate.sessionObject` beside its own
// objects, services and workflows:
//
//   import * as restate from 'restate-session-objects';
//
//   const notes = restate.sessionObject({
//     name: 'notes',
//     schema: ['CREATE TABLE notes (text TEXT NOT NULL)'],
//     handlers: {
//       add: async (ctx, text: string) => {
//         await ctx.db.execute('INSERT INTO notes (text) VALUES (?)', [text]);
//       },
//     },
//     views: {
//       list: async (ctx) => ctx.db.query<{ text: string }>('SELECT text FROM notes'),
//     },
//   });
//
// See README.md.

// The SDK, and what this module adds to it (or declares in its place: its
// `Client`, `SendClient` and `Target` are a session object's).
export * from '@restatedev/restate-sdk';
export type { Database, DatabaseReader, DatabaseWriter, Row, SqlParam, SqlValue, Transaction } from './session.ts';
export type { Step } from './schema.ts';
export { sessionObject, type Entry, type HandlerOptions, type SessionContext, type ViewContext } from './object.ts';
export type { CallOptions, Client, Rpc, Send, SendClient, SendOptions, Target } from './calls.ts';
