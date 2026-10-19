// A deadline: race a slow call against a timer. The call ends when its
// function settles, so the loser is dropped, not waited for.
//
//   curl localhost:8080/timeout/alice/run --json 300     # "done after 300 ms"
//   curl localhost:8080/timeout/alice/run --json 5000    # "timed out"
export async function run(ms, ctx) {
  const answer = ctx.rpc.call({ service: 'slow', handler: 'wait', key: 'timeout' }, ms);
  const deadline = ctx.sleep(1000).then(() => 'timed out');
  return await Promise.race([answer, deadline]);
}
