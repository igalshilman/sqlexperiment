// A long computation: count the primes below `n`. One call may compute for
// minutes without awaiting (QJS_TURN_TIMEOUT, 10 by default); what it found
// stays in memory for the next.
//
//   curl localhost:8080/primes/alice/count --json 100000    # 9592
//   curl localhost:8080/primes/alice/largest                # 99991
let biggest = 0;

export function count(n) {
  let found = 0;
  for (let i = 2; i < n; i++) {
    let prime = true;
    for (let d = 2; d * d <= i; d++) {
      if (i % d === 0) {
        prime = false;
        break;
      }
    }
    if (prime) {
      found++;
      biggest = Math.max(biggest, i);
    }
  }
  return found;
}

export function largest() {
  return biggest;
}
