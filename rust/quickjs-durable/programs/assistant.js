// A shop assistant: Claude, with the shop's programs as its tools. It looks
// up stock and the customer's balance, and buys through `checkout`. The
// conversation is the scope's: each run continues it.
//
// The agent loop is plain JavaScript. While it waits on Claude or on a tool,
// the loop and the conversation are saved with the rest of the heap: a crash
// resumes them, and a model call already answered is not made again.
//
//   ANTHROPIC_API_KEY=... target/release/quickjs-restate
//   curl localhost:8080/inventory/widget/run --json '{"restock": 5}'
//   curl localhost:8080/payments/alice/run --json '{"deposit": 100}'
//   curl localhost:8080/assistant/alice/run --json \
//     '{"customer": "alice", "message": "Buy two widgets, if I can afford them"}'
const MODEL = 'claude-opus-5-5';
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

// The tools, as calls to the shop's programs.
function shop(customer, ctx) {
  const call = (service, key, input) => ctx.rpc.call({ service, handler: 'run', key }, input);
  return {
    catalog: () =>
      Promise.all(
        Object.entries(PRICES).map(async ([sku, price]) => ({
          sku,
          price,
          stock: await call('inventory', sku, {}),
        })),
      ),
    balance: () => call('payments', customer, {}),
    async buy({ items }) {
      for (const { sku } of items) if (!(sku in PRICES)) throw new Error(`No such product: ${sku}`);
      const orders = ((await ctx.kv.get('orders')) ?? 0) + 1;
      await ctx.kv.set('orders', orders);
      return call('checkout', `${customer}-${orders}`, {
        customer,
        items: items.map(({ sku, qty }) => ({ sku, qty, price: PRICES[sku] })),
      });
    },
  };
}

async function main({ customer, message }, ctx) {
  const tools = shop(customer, ctx);
  const messages = (await ctx.kv.get('messages')) ?? [];
  messages.push({ role: 'user', content: message });
  for (let step = 0; step < 10; step++) {
    const response = await ctx.rpc.call(
      { service: 'claude', handler: 'messages' },
      {
        model: MODEL,
        max_tokens: 16000,
        output_config: { effort: 'low' },
        system: `You are the shop assistant of ${customer}. Use the tools to answer and to buy.`,
        tools: TOOLS,
        messages,
      },
    );
    if (response.stop_reason === 'refusal') return { answer: 'Sorry, I cannot help with that.' };
    messages.push({ role: 'assistant', content: response.content });
    if (response.stop_reason !== 'tool_use') {
      await ctx.kv.set('messages', messages);
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
    messages.push({ role: 'user', content: results });
  }
  throw new Error('The assistant took too many steps');
}
