// Concurrent awaits in a handler with SQL: one loop, the selector, drives
// every promise the handler awaits.
//
// The published SDK drives each awaited RestatePromise with a loop of its own,
// and only one of them may wait for progress from the server at a time
// (`ExternalProgressChannel.awaitNext`): two promises awaited at once that
// both need the server, such as two state reads with lazy state, fail the
// attempt with "awaitNext already pending". With journal replay, that order of
// awaits is part of what a replay must match; with explicit recovery nothing
// is replayed, so a handler with SQL hands every awaited promise to one loop,
// which asks the VM about all of them at once (`hookSelector`). The hook
// replaces the invocation's `promisesExecutor.doProgress`: it relies on the
// SDK's internals, as of the version this package pins (1.17.4).
//
// TODO: drop the selector, and the pin, once an SDK release lets a handler
// with explicit recovery await concurrently (FUTURE.md at the repository's root).
import { setImmediate } from 'node:timers/promises';

/** A future of the VM, as `do_progress` takes it. */
export type UnresolvedFuture =
  | { Single: number }
  | { FirstCompleted: UnresolvedFuture[] }
  | { AllCompleted: UnresolvedFuture[] }
  | { FirstSucceededOrAllFailed: UnresolvedFuture[] }
  | { AllSucceededOrFirstFailed: UnresolvedFuture[] }
  | { Unknown: UnresolvedFuture[] };

/** What `do_progress` returns. */
export type DoProgressResult =
  | 'AnyCompleted'
  | 'WaitExternalProgress'
  | { ExecuteRun: number }
  | 'CancelSignalReceived'
  | { EphemeralNotificationReady: number };

/** What the selector needs of a promise: the shape of InternalRestatePromise. */
export interface Selectable {
  /** Takes the result if it is ready, and settles the promise with it. */
  tryComplete(): Promise<void>;
  /** What it still waits for, or null once settled. */
  unresolvedFuture(): UnresolvedFuture | null;
  /** Rejects it with a CancelledError. */
  tryCancel(): void;
}

/** What the selector needs of the invocation. */
export interface SelectorDriver {
  doProgress(future: UnresolvedFuture): DoProgressResult;
  /** Writes out what the VM has buffered. */
  flushOutput(): Promise<void>;
  /** Resolves on the next signal of external progress (input, a finished run, a registration). */
  awaitExternalProgress(): Promise<void>;
  /** Signals external progress. */
  wake(): void;
  executeRun(handle: number): void;
  completeEphemeral(completionId: number): Promise<void>;
  /** The attempt cannot go on: suspension, or an error. */
  onError(e: unknown): void;
}

/**
 * One loop that drives every pending promise of an invocation, so that the
 * invocation has a single await point however many promises user code awaits
 * at once, with its own combinators or none.
 *
 * A promise is registered once and stays until it settles. The loop settles
 * whatever is ready, then asks the VM for progress on the first of all the
 * pending promises to complete: so a suspension waits for any of them, a
 * pending run starts whichever promise is awaited first, and a cancellation
 * reaches all of them. Registering wakes the loop, so a promise registered
 * while it waits is part of the next await point. An ephemeral command (a
 * state get left out of the journal) is registered by its completion id: the
 * VM reports its notification ahead of any future, and the loop goes on while
 * one is in flight, even with no promise pending. The loop runs while
 * something is pending, and is the only one waiting for external progress.
 */
export class PromiseSelector {
  private readonly pending = new Set<Selectable>();
  // The completion ids of the ephemeral commands in flight.
  private readonly ephemeral = new Set<number>();
  private running = false;
  private closed = false;
  // Between the VM's WaitExternalProgress and the next external progress.
  private waiting = false;

  private readonly driver: SelectorDriver;

  constructor(driver: SelectorDriver) {
    this.driver = driver;
  }

  register(promise: Selectable): void {
    if (this.closed || this.pending.has(promise)) return;
    this.pending.add(promise);
    this.start();
  }

  /** An ephemeral command's completion id, until its notification is ready. */
  registerEphemeral(completionId: number): void {
    if (this.closed) return;
    this.ephemeral.add(completionId);
    this.start();
  }

  private start(): void {
    if (this.running) {
      this.wake();
    } else {
      this.running = true;
      void this.loop();
    }
  }

  /** Stops the loop (it finds nothing pending), at the end of the attempt. */
  close(): void {
    if (this.closed) return;
    this.closed = true;
    this.pending.clear();
    this.ephemeral.clear();
    if (this.running) this.wake();
  }

  // Only a waiting loop needs waking, once: any other picks up the change on
  // its next pass. (Waking it otherwise would buffer a signal per promise, and
  // as many needless passes.)
  private wake(): void {
    if (!this.waiting) return;
    this.waiting = false;
    this.driver.wake();
  }

  /** The promises still pending, for tests. */
  get size(): number {
    return this.pending.size + this.ephemeral.size;
  }

  private async loop(): Promise<void> {
    try {
      await this.driver.flushOutput();
      for (;;) {
        // Settle what is ready.
        for (const promise of this.pending) {
          await promise.tryComplete();
        }
        // Let what that settled run, and register what it awaits next, so
        // that one check covers them (a combinator also flags itself
        // completed in a .finally, a tick late). An optimization: what is
        // registered later wakes the loop anyway.
        await setImmediate();

        const futures: UnresolvedFuture[] = [];
        for (const promise of this.pending) {
          const future = promise.unresolvedFuture();
          if (future === null) {
            this.pending.delete(promise);
          } else {
            futures.push(future);
          }
        }
        if (futures.length === 0 && this.ephemeral.size === 0) return;

        // With only ephemeral commands in flight, no future: the VM reports
        // their notifications, or waits for input.
        const result = this.driver.doProgress(futures.length === 1 ? futures[0]! : { FirstCompleted: futures });
        if (result === 'AnyCompleted') {
          continue;
        } else if (result === 'WaitExternalProgress') {
          // From here, in the same turn as the check: what is registered
          // after it is not part of it.
          this.waiting = true;
          await this.driver.flushOutput();
          await this.driver.awaitExternalProgress();
          this.waiting = false;
        } else if (result === 'CancelSignalReceived') {
          // Reaches every pending promise; what is awaited after it is not cancelled.
          for (const promise of this.pending) promise.tryCancel();
          this.pending.clear();
        } else if ('EphemeralNotificationReady' in result) {
          this.ephemeral.delete(result.EphemeralNotificationReady);
          await this.driver.completeEphemeral(result.EphemeralNotificationReady);
        } else {
          this.driver.executeRun(result.ExecuteRun);
          // Let the run start, then come back.
          await setImmediate();
        }
      }
    } catch (e) {
      this.close();
      this.driver.onError(e);
    } finally {
      // Decided with nothing pending, in the same turn: a registration from
      // here on starts a new loop.
      this.running = false;
    }
  }
}

/** The SDK's promises executor, as far as the hook needs it: its fields are private in its types. */
interface Executor {
  doProgress(promise: Selectable): Promise<void>;
  coreVm: {
    do_progress(future: UnresolvedFuture): DoProgressResult;
    take_ephemeral_notification(completionId: number): unknown;
  };
  outputPump: { awaitNextProgress(): Promise<void> };
  externalProgressChannel: { awaitNext(): Promise<void>; signal(): void };
  runClosuresTracker: { executeRun(handle: number): void };
  ephemeralRequestTracker: { complete(completionId: number, value: unknown): Promise<void> };
  errorCallback: (e: unknown) => void;
}

const FIELDS = [
  'coreVm',
  'outputPump',
  'externalProgressChannel',
  'runClosuresTracker',
  'ephemeralRequestTracker',
  'errorCallback',
] as const;

/**
 * Drive every promise that the invocation of `ctx` (the SDK's context, not a
 * proxy of it) awaits from here on with one selector. The returned function
 * stops it, at the end of the attempt, and gives the invocation back its
 * own loops.
 */
export function hookSelector(ctx: object): () => void {
  const executor = (ctx as { promisesExecutor?: Executor }).promisesExecutor;
  const missing = executor ? FIELDS.filter((field) => executor[field] === undefined) : ['promisesExecutor'];
  if (missing.length > 0 || typeof executor?.coreVm.do_progress !== 'function') {
    throw new Error(
      `restate-sessions drives a handler's promises through the SDK's internals, which this SDK version does not have ` +
        `(${missing.join(', ') || 'coreVm.do_progress'}): use the @restatedev/restate-sdk that restate-sessions pins`,
    );
  }
  const { coreVm, outputPump, externalProgressChannel, runClosuresTracker, ephemeralRequestTracker } = executor;
  const selector = new PromiseSelector({
    doProgress: (future) => coreVm.do_progress(future),
    flushOutput: () => outputPump.awaitNextProgress(),
    awaitExternalProgress: () => externalProgressChannel.awaitNext(),
    wake: () => externalProgressChannel.signal(),
    executeRun: (handle) => runClosuresTracker.executeRun(handle),
    completeEphemeral: (completionId) =>
      ephemeralRequestTracker.complete(completionId, coreVm.take_ephemeral_notification(completionId)),
    onError: (e) => executor.errorCallback(e),
  });
  // An own property, over the class's method: removing it gives the method back.
  executor.doProgress = async (promise) => selector.register(promise);
  return () => {
    selector.close();
    delete (executor as Partial<Executor>).doProgress;
  };
}
