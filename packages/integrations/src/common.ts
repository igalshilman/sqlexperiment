// What the integrations share.
import type * as restate from '@restatedev/restate-sdk';

/** A value, or what it is for an invocation. */
export type PerCall<T> = T | ((ctx: restate.SqlObjectContext) => T);

/** `value` for the invocation of `ctx`. */
export const forCall = <T>(value: PerCall<T>, ctx: restate.SqlObjectContext): T =>
  typeof value === 'function' ? (value as (ctx: restate.SqlObjectContext) => T)(ctx) : value;
