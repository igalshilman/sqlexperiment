// A shop assistant: a model, with the shop's functions as its tools. It looks
// up stock and the customer's balance, and buys through `checkout`. The key
// is the customer, and the conversation is in its database: each message
// continues it.
//
// The agent loop is plain JavaScript. While it waits on the model or a tool,
// the loop is saved with the rest of memory: a crash resumes it, and a model
// call already answered is not made again.
//
//   OPENAI_API_KEY=... target/release/functions
//   curl localhost:8080/inventory/widget/restock --json 5
//   curl localhost:8080/payments/alice/deposit --json 100
//   curl localhost:8080/assistant/alice/chat --json '"Buy two widgets, if I can afford them"'
const MODEL = 'openai/gpt-6-sol';
const PRICES = { widget: 10, gadget: 30, gizmo: 5 };

const TOOLS = [
  {
    name: 'catalog',
    description: 'The products for sale: their SKUs, prices and stock.',
    input_schema: { type: 'object', properties: {}, additionalProperties: false },
  },
  {
    name: 'balance',
    description: "The customer's account balance.",
    input_schema: { type: 'object', properties: {}, additionalProperties: false },
  },
  {
    name: 'buy',
    description:
      "Order products for the customer: reserves the stock and charges the customer's account. " +
      'Either the whole order goes through, or none of it.',
    strict: true,
    input_schema: {
      type: 'object',
      properties: {
        items: {
          type: 'array',
          items: {
            type: 'object',
            properties: { sku: { type: 'string' }, qty: { type: 'integer' } },
            required: ['sku', 'qty'],
            additionalProperties: false,
          },
        },
      },
      required: ['items'],
      additionalProperties: false,
    },
  },
];

// The tools, as calls to the shop's functions.
function shop(customer, ctx) {
  const call = (service, handler, key, input) => ctx.rpc.call({ service, handler, key }, input);
  return {
    catalog: () =>
      Promise.all(
        Object.entries(PRICES).map(async ([sku, price]) => ({
          sku,
          price,
          stock: await call('inventory', 'available', sku),
        })),
      ),
    balance: () => call('payments', 'current', customer),
    async buy({ items }) {
      for (const { sku } of items) if (!(sku in PRICES)) throw new Error(`No such product: ${sku}`);
      const orders = ((await ctx.kv.get('orders')) ?? 0) + 1;
      await ctx.kv.set('orders', orders);
      return call('checkout', 'pay', `${customer}-${orders}`, {
        customer,
        items: items.map(({ sku, qty }) => ({ sku, qty, price: PRICES[sku] })),
      });
    },
  };
}

export async function chat(message, ctx) {
  const customer = ctx.key;
  const tools = shop(customer, ctx);
  // The customer's conversation, in the scope's database: each chat
  // continues it.
  const conversation = ctx.llm.conversation('chat', {
    model: MODEL,
    system: `You are the shop assistant of ${customer}. Use the tools to answer and to buy.`,
    tools: TOOLS,
    options: { reasoning_effort: 'low' },
  });
  let next = { role: 'user', content: message };
  for (let step = 0; step < 10; step++) {
    const response = await conversation.send({ messages: [next] });
    if (response.stop_reason === 'refusal') return { answer: 'Sorry, I cannot help with that.' };
    if (response.stop_reason !== 'tool_use') {
      const answer = response.content.filter((block) => block.type === 'text');
      return { answer: answer.map((block) => block.text).join('\n') };
    }
    // Every tool the model asked for, at once; the results in one message.
    const uses = response.content.filter((block) => block.type === 'tool_use');
    const results = await Promise.all(
      uses.map(async ({ id, name, input }) => {
        try {
          const result = await tools[name](input);
          return { type: 'tool_result', tool_use_id: id, content: JSON.stringify(result) };
        } catch (error) {
          return { type: 'tool_result', tool_use_id: id, content: error.message, is_error: true };
        }
      }),
    );
    next = { role: 'user', content: results };
  }
  throw new Error('The assistant took too many steps');
}
