// A long computation: count the primes below `n`, recording progress in
// ctx.kv as it goes. A turn has a CPU budget; past half of it, the run is
// saved at its next wait and carries on in the next turn.
//
//   curl localhost:8080/primes/alice/run --json 100000
async function main(n, ctx) {
  let count = 0;
  for (let i = 2; i < n; i++) {
    let prime = true;
    for (let d = 2; d * d <= i; d++) {
      if (i % d === 0) {
        prime = false;
        break;
      }
    }
    if (prime) count++;
    if (i % 1000 === 0) await ctx.kv.set('progress', i);
  }
  return count;
}
