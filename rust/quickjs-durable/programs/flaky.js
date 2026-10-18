// An unreliable service: fails the first `failures` calls in its scope,
// then answers.
//
//   curl localhost:8080/flaky/a/run --json 2    # fails, fails, then "ok after 3 calls"
async function main(failures, ctx) {
  const calls = ((await ctx.kv.get('calls')) ?? 0) + 1;
  await ctx.kv.set('calls', calls);
  if (calls <= failures) throw new Error(`Unavailable (call ${calls})`);
  return `ok after ${calls} calls`;
}
