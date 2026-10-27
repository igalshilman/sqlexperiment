/**
 * The factory's environment, as `env.` in factory.ts: the real Linear, with
 * LINEAR_API_KEY (and LINEAR_WEBHOOK_SECRET, its webhook's signing secret),
 * or else the stand-in (linear.ts) on localhost:3333; the project on GitHub,
 * with GITHUB_REPO (owner/name, such as igalshilman/demos) and GITHUB_TOKEN,
 * or else a git repository standing in for it; and WORKSPACE, where the coders work
 * (/tmp/restate-factory by default).
 */
import { type Tool, tool, type WebhookAuth } from 'restate-agents';
import { z } from 'zod';
import { mcpTools } from './mcp.ts';
import { standInRemote, worktree } from './workspace.ts';

const apiKey = process.env.LINEAR_API_KEY;
const webhookSecret = process.env.LINEAR_WEBHOOK_SECRET;
const { GITHUB_REPO: repo, GITHUB_TOKEN: token } = process.env;
if (repo && !token) throw new Error(`GITHUB_REPO is ${repo}: set GITHUB_TOKEN too, a token that can push to it`);
const workspace = process.env.WORKSPACE ?? '/tmp/restate-factory';

/** Who wrote a comment or a pull request: the agent `who`, and its task, if it has one (`ENG-1.parser`). */
const signature = (who: string, key: string) => {
  const task = key.split('.').slice(1).join('.');
  return `**${who}${task ? ` · ${task}` : ''}**`;
};

/** Linear's tools (issues, sub-issues, states, comments), from its MCP server. */
const linear = apiKey
  ? await mcpTools('https://mcp.linear.app/mcp', { authorization: `Bearer ${apiKey}` })
  : await mcpTools('http://localhost:3333/mcp');

/**
 * Linear's tools for the agent `who` (such as "🧭 Triage"): each comment it
 * makes opens with its name, and its task, if it has one (`ENG-1.parser`:
 * "🛠️ Coder · parser"). The agents all comment as one Linear user: this
 * tells them apart.
 */
export const linearTools = (who: string): Tool[] =>
  linear.map((t) =>
    t.name !== 'create_comment'
      ? t
      : tool({
          name: t.name,
          description: t.description,
          input: t.input,
          run: ({ body, ...input }, call) =>
            t.run({ ...input, body: `${signature(who, call.ctx.key)}\n\n${String(body)}` }, call),
        }),
  );

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

/** The project's git repository: GitHub's (pushed to with GITHUB_TOKEN), or the stand-in's, in the workspace. */
const remote = repo ? `https://x-access-token:${token}@github.com/${repo}.git` : await standInRemote(workspace);

/** A coder's directory: a git worktree of the project, on a branch of its own (see workspace.ts). */
export const coderWorkspace = ({ key }: { key: string }) => worktree(workspace, remote, key);

/** GitHub's `create_pull_request`, from its MCP server. */
const createPullRequest = repo ? await githubTool('create_pull_request') : undefined;

async function githubTool(name: string) {
  const tools = await mcpTools('https://api.githubcopilot.com/mcp/', { authorization: `Bearer ${token}` });
  const found = tools.find((t) => t.name === name);
  if (!found) throw new Error(`GitHub's MCP server has no ${name} tool`);
  return found;
}

let standInPulls = 0;

/**
 * `create_pull_request` for the agent `who`: a pull request of the project,
 * whose description opens with who opened it. On the stand-in, a pull request
 * is only logged: the branches are in the stand-in's repository.
 */
export const pullRequestTool = (who: string) =>
  tool({
    name: 'create_pull_request',
    description: 'Open a pull request of the project: from the branch `head` into `base`. Answers with its link.',
    input: z.object({
      title: z.string(),
      body: z.string().describe('What it does, and how it was tested, in Markdown'),
      head: z.string().describe('The branch with the changes, pushed already'),
      base: z.string().describe('The branch it goes into'),
    }),
    run: async ({ title, body, head, base }, call) => {
      const described = `${signature(who, call.ctx.key)}\n\n${body}`;
      if (!createPullRequest) {
        console.log(`pull request #${++standInPulls}: ${head} → ${base}, "${title}"`);
        return `Pull request #${standInPulls}, ${head} → ${base} (on the stand-in: ${remote})`;
      }
      const [owner, name] = repo!.split('/');
      return createPullRequest.run({ owner, repo: name, title, body: described, head, base }, call);
    },
  });
