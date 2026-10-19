// An unreliable service: fails the first `failures` calls under its key,
// then answers.
//
//   curl localhost:8080/flaky/a/call --json 2    # fails, fails, then "ok after 3 calls"
export async function call(failures, ctx) {
  const calls = ((await ctx.kv.get('calls')) ?? 0) + 1;
  await ctx.kv.set('calls', calls);
  if (calls <= failures) throw new Error(`Unavailable (call ${calls})`);
  return `ok after ${calls} calls`;
}
