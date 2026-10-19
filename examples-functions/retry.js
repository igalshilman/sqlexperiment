// Retries with exponential backoff: call `flaky` until it answers, waiting
// 100 ms, 200 ms, 400 ms... between attempts, up to `attempts` times. The
// waits are durable timers: a crash in between loses nothing.
//
// Each run calls a scope of `flaky` of its own, named with Math.random(): the
// run's generator is saved with it, so a retried attempt picks the same one.
//
//   curl localhost:8080/retry/a/run --json '{"failures": 3, "attempts": 5}'
async function main({ failures, attempts }, ctx) {
  const scope = Math.random().toString(36).slice(2);
  const flaky = { service: 'flaky', handler: 'run', key: scope };
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
