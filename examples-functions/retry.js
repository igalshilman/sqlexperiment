// Retries with exponential backoff: call `flaky` until it answers, waiting
// 100 ms, 200 ms, 400 ms... between attempts, up to `attempts` times. The
// waits are durable timers: a crash in between loses nothing.
//
// Each run calls a `flaky` of its own, its key from Math.random(): the
// generator is saved with the call, so a retried attempt picks the same one.
//
//   curl localhost:8080/retry/a/run --json '{"failures": 3, "attempts": 5}'
export async function run({ failures, attempts }, ctx) {
  const flaky = { service: 'flaky', handler: 'call', key: Math.random().toString(36).slice(2) };
  const errors = [];
  for (let attempt = 0; attempt < attempts; attempt++) {
    try {
      return { answer: await ctx.rpc.call(flaky, failures), errors };
    } catch (error) {
      errors.push(error.message);
      await ctx.sleep(100 * 2 ** attempt);
    }
  }
  throw new Error(`Gave up after ${attempts} attempts`);
}
