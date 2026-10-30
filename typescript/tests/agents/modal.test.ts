// DeepSeek's agent on a model of Modal's endpoint (`modal/<model-id>`): the
// harness asks the endpoint for OpenAI's Chat Completions, with a reasoning
// effort, the proxy token and the conversation's routing affinity. A stand-in
// for the endpoint answers.
import { createServer, type IncomingHttpHeaders } from 'node:http';
import type { AddressInfo } from 'node:net';
import * as deepseek from 'restate-agents/deepseek';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { conversation } from '../testing.ts';

/** A Chat Completions stream that answers `text`. */
const answering = (text: string) =>
  [
    { choices: [{ index: 0, delta: { role: 'assistant', content: '' }, finish_reason: null }] },
    { choices: [{ index: 0, delta: { content: text }, finish_reason: null }] },
    {
      choices: [{ index: 0, delta: {}, finish_reason: 'stop' }],
      usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 },
    },
  ]
    .map(
      (chunk) =>
        `data: ${JSON.stringify({ id: 'chatcmpl-1', object: 'chat.completion.chunk', created: 0, model: 'm', ...chunk })}\n\n`,
    )
    .join('')
    .concat('data: [DONE]\n\n');

/** What the stand-in was asked: each request's path, headers, model and reasoning effort. */
const asked: { path: string; headers: IncomingHttpHeaders; model: string; effort: string }[] = [];

const endpoint = createServer((req, res) => {
  let body = '';
  req.on('data', (chunk) => (body += chunk));
  req.on('end', () => {
    const { model, reasoning_effort } = JSON.parse(body);
    asked.push({ path: req.url!, headers: req.headers, model, effort: reasoning_effort });
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
        path: '/v1/chat/completions',
        model: 'deepseek-ai/DeepSeek-V4.1-Flash',
        effort: 'high',
        headers: expect.objectContaining({
          authorization: 'Bearer wk-test.ws-test',
          'modal-routing-affinity-key': 'ENG-1.plan',
        }),
      },
    ]);
  });
});
