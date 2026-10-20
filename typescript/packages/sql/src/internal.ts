// For tests: an attempt's session over a context of their own, and the
// store's fault injection.
export { AttemptEnd, setStoreWrapper, SqlAttempt, SqlErrors, withSql } from './session.ts';
export type { Outcome, SqlHost, SqlStore } from './session.ts';
