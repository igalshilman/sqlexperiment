/**
 * The factory's environment, as `env.` in factory.ts: the real Linear, with
 * LINEAR_API_KEY (and LINEAR_WEBHOOK_SECRET, its webhook's signing secret),
 * or else the stand-in (linear.ts) on localhost:3333; and WORKSPACE, where
 * the coders work (/tmp/restate-factory by default).
 */
import type { WebhookAuth } from 'restate-agents';
import { mcpTools } from './mcp.ts';
import { commitTool, issueDirectory } from './workspace.ts';

const apiKey = process.env.LINEAR_API_KEY;
const webhookSecret = process.env.LINEAR_WEBHOOK_SECRET;
const workspace = process.env.WORKSPACE ?? '/tmp/restate-factory';

/** Linear's tools (issues, sub-issues, states, comments), from its MCP server. */
export const linearTools = () =>
  apiKey
    ? mcpTools('https://mcp.linear.app/mcp', { authorization: `Bearer ${apiKey}` })
    : mcpTools('http://localhost:3333/mcp');

/**
 * The factory's own Linear user (`viewer`, the user of LINEAR_API_KEY), or the
 * stand-in's `factory`: the comments it makes come back as webhooks, and are
 * not for triage. With a personal API key, it is you: then only an app's
 * token (an OAuth app installed with `actor=app`) tells your comments apart.
 */
export const linearUser: string = apiKey ? await viewerOf(apiKey) : 'factory';

async function viewerOf(key: string) {
  const response = await fetch('https://api.linear.app/graphql', {
    method: 'POST',
    // A personal API key as it is; an OAuth token (an app's) as a bearer token.
    headers: { 'content-type': 'application/json', authorization: key.startsWith('lin_api_') ? key : `Bearer ${key}` },
    body: JSON.stringify({ query: '{ viewer { id name } }' }),
  });
  const { data } = (await response.json()) as { data?: { viewer: { id: string; name: string } } };
  if (!data) throw new Error(`LINEAR_API_KEY: Linear answers ${response.status}`);
  console.log(`The factory acts in Linear as ${data.viewer.name}`);
  return data.viewer.id;
}

/** How Linear signs its webhooks, if there is a secret to check them with. */
export const webhookAuth: WebhookAuth | undefined = webhookSecret
  ? { header: 'linear-signature', secret: webhookSecret }
  : undefined;

/** A coder's directory: its issue's, in the workspace, shared by the issue's coders. */
export const issueWorkspace = ({ key }: { key: string }) => issueDirectory(workspace, key);

/** `commit`: a coder's files, as its own commit, in its issue's directory. */
export const commit = commitTool(workspace);
