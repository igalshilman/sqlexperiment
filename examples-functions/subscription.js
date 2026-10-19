// A subscription: charge the customer `price` every `every` ms, `periods`
// times, and stop at the first declined charge. Each wait is a durable
// timer, however long the period. The key is the customer.
//
//   curl localhost:8080/payments/alice/deposit --json 25
//   curl localhost:8080/subscription/alice/start --json '{"price": 10, "periods": 3, "every": 1000}'
//   curl localhost:8080/subscription/alice/charges
const charged = [];

export async function start({ price, periods, every }, ctx) {
  const account = { service: 'payments', handler: 'charge', key: ctx.key };
  for (let period = 1; period <= periods; period++) {
    try {
      charged.push({ period, balance: await ctx.rpc.call(account, price) });
    } catch (error) {
      return { status: 'lapsed', charges: charged, reason: error.message };
    }
    if (period < periods) await ctx.sleep(every);
  }
  return { status: 'complete', charges: charged };
}

export function charges() {
  return charged;
}
