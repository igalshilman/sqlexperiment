/**
 * The factory's environment: the real Linear, with LINEAR_API_KEY (and
 * LINEAR_WEBHOOK_SECRET, its webhook's signing secret), or else the stand-in
 * (linear.ts) on localhost:3333; and WORKSPACE, where the coders work.
 */
import type { WebhookAuth } from 'restate-agents';
import { mcpTools } from './mcp.ts';

const apiKey = process.env.LINEAR_API_KEY;
const webhookSecret = process.env.LINEAR_WEBHOOK_SECRET;

/** Where the coders work: a directory per issue in it. */
export const workspace = process.env.WORKSPACE ?? '/tmp/restate-factory';

/** Linear's tools (issues, sub-issues, states, comments), from its MCP server. */
export const linearTools = () =>
  apiKey
    ? mcpTools('https://mcp.linear.app/mcp', { authorization: `Bearer ${apiKey}` })
    : mcpTools('http://localhost:3333/mcp');

/** How Linear signs its webhooks, if there is a secret to check them with. */
export const linearSignature: WebhookAuth | undefined = webhookSecret
  ? { header: 'linear-signature', secret: webhookSecret }
  : undefined;
