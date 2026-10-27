/**
 * A stand-in for Linear: issues and sub-issues in memory, the tools of
 * Linear's MCP server that the factory uses, and a webhook to the factory for
 * each new issue and comment.
 *
 *   node advanced/factory/linear.ts        # on port 3333: its MCP server at /mcp
 *   curl localhost:3333/issues --json '{"title": "…", "description": "…"}'   # a new issue, and its webhook
 *   curl localhost:3333/issues/ENG-1/comments --json '{"body": "How is it going?"}'   # a comment, and its webhook
 *   curl localhost:3333/issues/ENG-1       # the issue: its state, history, sub-issues and comments
 *
 * You are the user `you`; the factory, through the MCP server, is the user
 * `factory`. A webhook says who acted (its `actor`), as Linear's do.
 *
 * The tools are named as Linear's (`get_issue`, `create_issue`,
 * `update_issue`, `create_comment`, `list_issue_statuses`), and its states
 * are a new team's. A webhook goes to WEBHOOK (Restate's ingress, by
 * default), in Linear's shape, signed with LINEAR_WEBHOOK_SECRET if there is
 * one, as Linear signs them.
 *
 * Environment: PORT, WEBHOOK, LINEAR_WEBHOOK_SECRET.
 */
import { createHmac, randomUUID } from 'node:crypto';
import { createServer, type IncomingMessage, type ServerResponse } from 'node:http';
import { Readable } from 'node:stream';
import { createMcpHandler, McpServer } from '@modelcontextprotocol/server';
import { z } from 'zod';

const port = Number(process.env.PORT ?? 3333);
const webhook = process.env.WEBHOOK ?? 'http://localhost:8080/linear/webhook';
const secret = process.env.LINEAR_WEBHOOK_SECRET;

/** A new team's workflow, as Linear makes it: an issue moves through these states. */
const STATES = ['Backlog', 'Todo', 'In Progress', 'In Review', 'Done', 'Canceled'] as const;

interface Issue {
  id: string;
  identifier: string;
  title: string;
  description: string;
  /** The issue this one is a sub-issue of. */
  parentId: string | null;
  children: string[];
  state: string;
  history: { state: string; at: string }[];
  comments: { body: string; by: User; at: string }[];
}

/** Who acts: you, through the HTTP API, or the factory, through the MCP server. */
type User = 'you' | 'factory';

const issues = new Map<string, Issue>();

const issueOf = (id: string) => {
  const issue = issues.get(id);
  if (!issue) throw new Error(`No issue ${id}`);
  return issue;
};

/** Linear's webhook of `type` (Issue, Comment) for `data`, made by `by`, signed as Linear signs them. */
async function sendWebhook(type: string, data: object, by: User) {
  const actor = { id: by, name: by, type: 'user' };
  const body = JSON.stringify({ action: 'create', type, actor, data, webhookTimestamp: Date.now() });
  const headers: Record<string, string> = {
    'content-type': 'application/json',
    'linear-event': type,
    'linear-delivery': randomUUID(),
  };
  if (secret) headers['linear-signature'] = createHmac('sha256', secret).update(body).digest('hex');
  try {
    const response = await fetch(webhook, { method: 'POST', headers, body });
    if (!response.ok) throw new Error(`${response.status} ${await response.text()}`);
  } catch (error) {
    console.log(`the webhook of a new ${type} failed: ${String(error)}`);
  }
}

/** A new issue, in the backlog, or a sub-issue of `parentId`: Linear sends its webhook. */
async function createIssue(
  title: string,
  description: string,
  by: User,
  parentId: string | null = null,
): Promise<Issue> {
  if (parentId) issueOf(parentId);
  const identifier = `ENG-${issues.size + 1}`;
  const at = new Date().toISOString();
  const issue: Issue = {
    id: randomUUID(),
    identifier,
    title,
    description,
    parentId,
    children: [],
    state: 'Backlog',
    history: [{ state: 'Backlog', at }],
    comments: [],
  };
  issues.set(identifier, issue);
  if (parentId) issueOf(parentId).children.push(identifier);
  console.log(`${identifier}: created, "${title}"${parentId ? `, a sub-issue of ${parentId}` : ''}`);

  await sendWebhook('Issue', { id: issue.id, identifier, title, description, parentId }, by);
  return issue;
}

/** A comment on the issue `id`, by `by`: Linear sends its webhook, with the issue. */
async function createComment(id: string, body: string, by: User) {
  const issue = issueOf(id);
  issue.comments.push({ body, by, at: new Date().toISOString() });
  console.log(`${id}: ${by} commented, "${body.split('\n')[0]}"`);
  const about = { id: issue.id, identifier: issue.identifier, title: issue.title };
  await sendWebhook('Comment', { id: randomUUID(), body, userId: by, issueId: issue.id, issue: about }, by);
  return issue.comments.length;
}

const text = (value: unknown) => ({ content: [{ type: 'text' as const, text: JSON.stringify(value) }] });

/** Linear's MCP server, the tools the factory uses. */
function linear() {
  const server = new McpServer({ name: 'linear', version: '1.0.0' });

  server.registerTool(
    'get_issue',
    {
      description: 'An issue: its title, description, state, sub-issues and comments.',
      inputSchema: z.object({ id: z.string().describe('The issue, such as ENG-1') }),
    },
    async ({ id }) => text(issueOf(id)),
  );

  server.registerTool(
    'create_issue',
    {
      description: 'Create an issue, in the backlog: a sub-issue, with a parent.',
      inputSchema: z.object({
        title: z.string(),
        description: z.string().optional(),
        parentId: z.string().optional().describe('The parent issue, such as ENG-1'),
      }),
    },
    async ({ title, description, parentId }) => {
      const { identifier } = await createIssue(title, description ?? '', 'factory', parentId);
      return text({ identifier });
    },
  );

  server.registerTool(
    'list_issue_statuses',
    { description: "The team's issue states, in the order an issue moves through them.", inputSchema: z.object({}) },
    async () => text(STATES),
  );

  server.registerTool(
    'update_issue',
    {
      description: "Move an issue to another state of the team's workflow.",
      inputSchema: z.object({ id: z.string().describe('The issue, such as ENG-1'), state: z.enum(STATES) }),
    },
    async ({ id, state }) => {
      const issue = issueOf(id);
      if (issue.state !== state) {
        console.log(`${id}: ${issue.state} → ${state}`);
        issue.state = state;
        issue.history.push({ state, at: new Date().toISOString() });
      }
      return text({ id, state });
    },
  );

  server.registerTool(
    'create_comment',
    {
      description: 'Comment on an issue, in Markdown.',
      inputSchema: z.object({ issueId: z.string().describe('The issue, such as ENG-1'), body: z.string() }),
    },
    async ({ issueId, body }) => text({ issueId, comments: await createComment(issueId, body, 'factory') }),
  );

  return server;
}

const mcp = createMcpHandler(linear);

/** Linear's API, the part the demo uses, and its MCP server. */
async function handle(req: IncomingMessage, res: ServerResponse) {
  const url = new URL(req.url!, `http://localhost:${port}`);
  const body = req.method === 'GET' || req.method === 'HEAD' ? undefined : Buffer.concat(await req.toArray());
  const json = (status: number, value: unknown) =>
    res.writeHead(status, { 'content-type': 'application/json' }).end(JSON.stringify(value, null, 2));

  if (url.pathname === '/mcp') {
    const headers = req.headers as Record<string, string>;
    const response = await mcp.fetch(new Request(url, { method: req.method, headers, body }));
    res.writeHead(response.status, Object.fromEntries(response.headers));
    if (response.body) Readable.fromWeb(response.body).pipe(res);
    else res.end();
  } else if (url.pathname === '/issues' && req.method === 'POST') {
    const { title, description = '' } = JSON.parse(String(body));
    json(201, await createIssue(title, description, 'you'));
  } else if (/^\/issues\/[^/]+\/comments$/.test(url.pathname) && req.method === 'POST') {
    const id = url.pathname.split('/')[2]!;
    const { body: comment } = JSON.parse(String(body));
    if (issues.has(id)) json(201, { comments: await createComment(id, comment, 'you') });
    else json(404, { error: 'no such issue' });
  } else if (url.pathname.startsWith('/issues/')) {
    const issue = issues.get(url.pathname.slice('/issues/'.length));
    json(issue ? 200 : 404, issue ?? { error: 'no such issue' });
  } else {
    json(404, { error: 'not found' });
  }
}

createServer((req, res) => {
  handle(req, res).catch((error: unknown) => res.writeHead(500).end(String(error)));
}).listen(port, () => console.log(`Linear (a stand-in) on port ${port}: MCP at /mcp, webhooks to ${webhook}`));
