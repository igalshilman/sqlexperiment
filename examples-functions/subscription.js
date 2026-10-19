// A subscription: charge the customer `price` every `every` ms, `periods`
// times, and stop at the first declined charge. Each wait is a durable
// timer, however long the period.
//
//   curl localhost:8080/payments/alice/run --json '{"deposit": 25}'
//   curl localhost:8080/subscription/alice/run --json \
//     '{"customer": "alice", "price": 10, "periods": 3, "every": 1000}'
async function main({ customer, price, periods, every }, ctx) {
  const account = { service: 'payments', handler: 'run', key: customer };
  const charges = [];
  for (let period = 1; period <= periods; period++) {
    try {
      const balance = await ctx.rpc.call(account, { charge: price });
      charges.push({ period, balance });
    } catch (error) {
      return { status: 'lapsed', charges, reason: error.message };
    }
    if (period < periods) await ctx.sleep(every);
  }
  return { status: 'complete', charges };
}
