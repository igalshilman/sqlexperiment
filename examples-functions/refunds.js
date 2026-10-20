// A refund that waits for a person: it asks a manager to approve it, waits
// (for minutes or for days: nothing runs, and a restart changes nothing),
// then pays the customer back, through `payments`. The key is the customer.
//
//   curl localhost:8080/payments/alice/deposit --json 100
//   curl localhost:8080/refunds/alice/request/send --json '{"amount": 30, "reason": "arrived broken"}'
//   curl localhost:8080/refunds/alice/pending                                # what it waits for
//   curl localhost:8080/refunds/alice/signal --json '{"name": "approval", "value": "approved"}'
//   curl localhost:8080/payments/alice/current                               # 130
export async function request({ amount, reason }, ctx) {
  const verdict = await ctx.signal('approval', { amount, reason });
  if (verdict !== 'approved') return { refunded: false, verdict };
  const balance = await ctx.rpc.call({ service: 'payments', handler: 'refund', key: ctx.key }, amount);
  return { refunded: true, balance };
}
