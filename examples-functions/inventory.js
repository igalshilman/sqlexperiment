// Stock of one product (the key is its SKU). Reserving more than is in stock
// fails, and changes nothing.
//
//   curl localhost:8080/inventory/widget/restock --json 5
//   curl localhost:8080/inventory/widget/reserve --json 2    # 3 left
export async function restock(qty, ctx) {
  return change(qty, ctx);
}

export async function reserve(qty, ctx) {
  return change(-qty, ctx);
}

export async function release(qty, ctx) {
  return change(qty, ctx);
}

export async function available(_, ctx) {
  return (await ctx.kv.get('stock')) ?? 0;
}

async function change(by, ctx) {
  const stock = (await ctx.kv.get('stock')) ?? 0;
  if (stock + by < 0) throw new Error(`Out of stock: ${stock} left, ${-by} wanted`);
  await ctx.kv.set('stock', stock + by);
  return stock + by;
}
