// DeepSeek's agent on a model of Modal's endpoint (`modal/<model-id>`): the
// harness asks the endpoint for Anthropic's Messages, with the proxy token and
// the conversation's routing affinity. A stand-in for the endpoint answers.
import { createServer, type IncomingHttpHeaders } from 'node:http';
import type { AddressInfo } from 'node:net';
import * as deepseek from 'restate-agents/deepseek';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { conversation } from '../testing.ts';

/** A Messages stream that answers `text`. */
const answering = (text: string) =>
  [
    {
      type: 'message_start',
      message: {
        id: 'msg_1',
        type: 'message',
        role: 'assistant',
        content: [],
        usage: { input_tokens: 1, output_tokens: 0 },
      },
    },
    { type: 'content_block_start', index: 0, content_block: { type: 'text', text: '' } },
    { type: 'content_block_delta', index: 0, delta: { type: 'text_delta', text } },
    { type: 'content_block_stop', index: 0 },
    { type: 'message_delta', delta: { stop_reason: 'end_turn' }, usage: { output_tokens: 1 } },
    { type: 'message_stop' },
  ]
    .map((event) => `event: ${event.type}\ndata: ${JSON.stringify(event)}\n\n`)
    .join('');

/** What the stand-in was asked: each request's path, headers and model. */
const asked: { path: string; headers: IncomingHttpHeaders; model: string }[] = [];

const endpoint = createServer((req, res) => {
  let body = '';
  req.on('data', (chunk) => (body += chunk));
  req.on('end', () => {
    asked.push({ path: req.url!, headers: req.headers, model: JSON.parse(body).model });
    res.writeHead(200, { 'content-type': 'text/event-stream' }).end(answering('Hello from Modal'));
  });
});

beforeAll(async () => {
  await new Promise<void>((listening) => endpoint.listen(0, '127.0.0.1', listening));
  process.env.MODAL_ENDPOINT_URL = `http://127.0.0.1:${(endpoint.address() as AddressInfo).port}`;
  process.env.MODAL_PROXY_KEY = 'wk-test';
  process.env.MODAL_PROXY_SECRET = 'ws-test';
});

afterAll(() => {
  endpoint.close();
  delete process.env.MODAL_ENDPOINT_URL;
  delete process.env.MODAL_PROXY_KEY;
  delete process.env.MODAL_PROXY_SECRET;
});

describe("DeepSeek's agent on Modal's endpoint", () => {
  it('asks the endpoint, with the proxy token, keeping the conversation on one container', async () => {
    const chat = deepseek.chat({ model: 'modal/deepseek-ai/DeepSeek-V4.1-Flash' });
    expect(await conversation(chat, 'ENG-1.plan').attempt('inv1', 'Hi')).toBe('Hello from Modal');
    expect(asked).toEqual([
      {
        path: '/v1/messages',
        model: 'deepseek-ai/DeepSeek-V4.1-Flash',
        headers: expect.objectContaining({
          authorization: 'Bearer wk-test.ws-test',
          'modal-routing-affinity-key': 'ENG-1.plan',
        }),
      },
    ]);
  });
});
