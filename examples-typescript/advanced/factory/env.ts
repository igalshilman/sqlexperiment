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
import { pullRequestMark } from './github.ts';
import { mcpTools } from './mcp.ts';
import { signer } from './signatures.ts';
import { clone, project, pushed, standInRemote } from './workspace.ts';

const apiKey = process.env.LINEAR_API_KEY;
const webhookSecret = process.env.LINEAR_WEBHOOK_SECRET;
const { GITHUB_REPO: repo, GITHUB_TOKEN: token } = process.env;
if (repo && !token) throw new Error(`GITHUB_REPO is ${repo}: set GITHUB_TOKEN too, a token that can push to it`);
const workspace = process.env.WORKSPACE ?? '/tmp/restate-factory';

/**
 * `make()`, begun now and awaited where it is used: the endpoint does not
 * wait for it to start serving (a coder's sandbox starts cold, on each new
 * coder). If it fails, it is made again when next used.
 */
function begun<T>(what: string, make: () => Promise<T>): () => Promise<T> {
  let made = make();
  made.catch((e: Error) => console.warn(`${what}: ${e.message} (made again when next used)`));
  return () => (made = made.catch(make));
}

/** The project's git repository: GitHub's (pushed to with GITHUB_TOKEN), or the stand-in's, in the workspace. */
const remote = begun('the project', async () =>
  project(repo ? `https://github.com/${repo}.git` : await standInRemote(workspace)),
);

/** GitHub's tools, from its MCP server: those the factory uses, when a tool needs them. */
const github = begun("GitHub's tools", () =>
  repo ? mcpTools('https://api.githubcopilot.com/mcp/', { authorization: `Bearer ${token}` }) : Promise.resolve([]),
);

/**
 * Linear's tools (issues, sub-issues, states, comments), from its MCP server.
 * Awaited before serving, the only one: they are the agents' tools, which
 * their definitions name.
 */
const linear = apiKey
  ? await mcpTools('https://mcp.linear.app/mcp', { authorization: `Bearer ${apiKey}` })
  : await mcpTools('http://localhost:3333/mcp');
console.log(`Linear's tools: ${linear.map((t) => t.name).join(', ')}`);

/**
 * Linear's tools for the agent `who` (such as "🧭 Triage"): what it writes,
 * each comment and each issue it creates, opens with its name, and its task,
 * if it has one (`ENG-1.parser`: "🛠️ Coder · parser"). The agents all write
 * as one Linear user: this tells them apart.
 */
export const linearTools = (who: string): Tool[] => {
  const sign = signer(who);
  return linear.map((t) =>
    tool({
      name: t.name,
      description: t.description,
      input: t.input,
      run: (input, call) => {
        const field = writing(t, input);
        if (!field) return t.run(input, call);
        return t.run({ ...input, [field]: sign(call.ctx.key, String(input[field])) }, call);
      },
    }),
  );
};

/**
 * The field of a Linear tool's input that an agent writes, to sign: a
 * comment's `body`, or a new issue's `description` (an issue with an `id` is
 * updated: it is signed already). By the input, not the tool's name: Linear
 * names its tools as it likes.
 */
function writing(t: Tool, input: Record<string, unknown>) {
  const fields = t.input.shape;
  if (/comment/.test(t.name) && 'body' in fields && typeof input.body === 'string') return 'body';
  if (/issue/.test(t.name) && 'description' in fields && typeof input.description === 'string' && !('id' in input))
    return 'description';
  return undefined;
}

/** How Linear signs its webhooks, if there is a secret to check them with. */
export const linearWebhookAuth: WebhookAuth | undefined = webhookSecret
  ? { header: 'linear-signature', secret: webhookSecret }
  : undefined;

/** A coder's directory: a clone of the project, on a branch of its own (see workspace.ts). */
export const coderWorkspace = async ({ key }: { key: string }) => clone(workspace, await remote(), key);

/** GitHub's tool `name`. */
const githubTool = async (name: string) => {
  const found = (await github()).find((t) => t.name === name);
  if (!found) throw new Error(`GitHub's MCP server has no ${name} tool`);
  return found;
};

let standInPulls = 0;

/**
 * `create_pull_request` for the agent `who`: a pull request of the project,
 * whose description opens with who opened it, and ends with a mark of its
 * coder (hidden, for GitHub's webhook: see github.ts). On the stand-in, a pull request
 * is only logged: the branches are in the stand-in's repository.
 */
export const pullRequestTool = (who: string) => {
  const sign = signer(who);
  return tool({
    name: 'create_pull_request',
    description: 'Open a pull request of the project: from the branch `head` into `base`. Answers with its link.',
    input: z.object({
      title: z.string(),
      body: z.string().describe('What it does, and how it was tested, in Markdown'),
      head: z.string().describe('The branch with the changes, pushed already'),
      base: z.string().describe('The branch it goes into'),
    }),
    run: async ({ title, body, head, base }, call) => {
      const described = `${sign(call.ctx.key, body)}\n\n${pullRequestMark(call.ctx.key)}`;
      if (!repo) {
        console.log(`pull request #${++standInPulls}: ${head} → ${base}, "${title}"`);
        return `Pull request #${standInPulls}, ${head} → ${base} (on the stand-in: ${await remote()})`;
      }
      const [owner, name] = repo.split('/');
      const createPullRequest = await githubTool('create_pull_request');
      return createPullRequest.run({ owner, repo: name, title, body: described, head, base }, call);
    },
  });
};

/** What a read of a pull request answers: what it read, or why it could not. */
const reading = (read: () => Promise<string>) =>
  read().catch((e: Error) => `Could not read it (pushed yet?): ${e.message}`);

/** The pull request the reviewer `key` reviews: its coder's, of the same key (`ENG-1.parser`), as pushed. */
const pullRequest = (key: string) => pushed(workspace, key);

/**
 * The reviewer's tools, for the agent `who`: the pull request it reviews, as
 * pushed (its coder's: `reviewer/ENG-1.parser` reviews `coder/ENG-1.parser`'s),
 * and a comment on it, its review.
 */
export const reviewTools = (who: string): Tool[] => [
  tool({
    name: 'pull_request_diff',
    description: "The pull request's changes, as pushed: the files it changes, and their diff against main",
    input: z.object({}),
    run: (_, call) => reading(() => pullRequest(call.ctx.key).changes()),
  }),
  tool({
    name: 'read_file',
    description: 'A file of the pull request, as pushed, by its path in the project',
    input: z.object({ path: z.string() }),
    run: ({ path }, call) => reading(() => pullRequest(call.ctx.key).file(path)),
  }),
  pullRequestCommentTool(who),
];

/**
 * `comment_on_pull_request` for the agent `who`: a comment on a pull request,
 * signed, such as a review, or a coder's answer to one. On the stand-in, a
 * comment is only logged.
 */
export const pullRequestCommentTool = (who: string) => {
  const sign = signer(who);
  return tool({
    name: 'comment_on_pull_request',
    description: 'Comment on a pull request: a review, or an answer to one',
    input: z.object({
      number: z.number().int().describe("The pull request's number"),
      body: z.string().describe('The comment, in Markdown'),
    }),
    run: async ({ number, body }, call) => {
      const signed = sign(call.ctx.key, body);
      if (!repo) {
        console.log(`comment on pull request #${number}:\n${signed}`);
        return 'Posted (on the stand-in: logged)';
      }
      const [owner, name] = repo.split('/');
      const addComment = await githubTool('add_issue_comment');
      return addComment.run({ owner, repo: name, issue_number: number, body: signed }, call);
    },
  });
};
