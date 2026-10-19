// A long computation: count the primes below `n`, recording progress in
// ctx.kv as it goes. Past a second of work, the call is saved at its next
// wait and carries on in the next turn.
//
//   curl localhost:8080/primes/alice/count --json 100000    # 9592
//   curl localhost:8080/primes/alice/progress               # 99000
export async function count(n, ctx) {
  let found = 0;
  for (let i = 2; i < n; i++) {
    let prime = true;
    for (let d = 2; d * d <= i; d++) {
      if (i % d === 0) {
        prime = false;
        break;
      }
    }
    if (prime) found++;
    if (i % 1000 === 0) await ctx.kv.set('progress', i);
  }
  return found;
}

export async function progress(_, ctx) {
  return (await ctx.kv.get('progress')) ?? 0;
}
