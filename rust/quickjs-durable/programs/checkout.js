// A saga: reserve every item, then charge the customer. When a step fails,
// undo the steps done so far, in reverse, and report why. The scope is the
// order: checking out the same order again returns its first outcome.
//
//   curl localhost:8080/checkout/order-1/run --json \
//     '{"customer": "alice", "items": [{"sku": "widget", "qty": 2, "price": 10}]}'
async function main({ customer, items }, ctx) {
  const done = await ctx.kv.get('outcome');
  if (done) return done;

  const inventory = (sku) => ({ service: 'inventory', handler: 'run', key: sku });
  const payments = { service: 'payments', handler: 'run', key: customer };
  const undo = [];
  let outcome;
  try {
    for (const { sku, qty } of items) {
      await ctx.rpc.call(inventory(sku), { reserve: qty });
      undo.push(() => ctx.rpc.call(inventory(sku), { release: qty }));
    }
    const total = items.reduce((sum, { qty, price }) => sum + qty * price, 0);
    await ctx.rpc.call(payments, { charge: total });
    outcome = { status: 'paid', total };
  } catch (error) {
    for (const step of undo.reverse()) await step();
    outcome = { status: 'cancelled', reason: error.message };
  }
  await ctx.kv.set('outcome', outcome);
  return outcome;
}
