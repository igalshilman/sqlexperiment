/**
 * GitHub's webhook, into the coders: which coder an event on a pull request is
 * for, and what of it the coder reads. The events: a comment on a pull
 * request (in its conversation, or on a line of its code), a review, and a CI
 * run that failed; the agents' own comments are dropped (signatures.ts).
 * With GITHUB_WEBHOOK_SECRET, the webhook's secret, an event GitHub did not
 * sign is refused. GitHub's events: https://docs.github.com/en/webhooks/webhook-events-and-payloads
 */
import type { WebhookAuth, WebhookEvent } from 'restate-agents';
import { byAnAgent } from './signatures.ts';
import { coderOfBranch } from './workspace.ts';

const secret = process.env.GITHUB_WEBHOOK_SECRET;

/** How GitHub signs its webhooks, if there is a secret to check them with. */
export const webhookAuth: WebhookAuth | undefined = secret ? { header: 'x-hub-signature-256', secret } : undefined;

/**
 * The mark of the coder `key` (`ENG-1.parser`), at the end of its pull
 * request's description, hidden: a comment's event has the pull request's
 * description, not its branch.
 */
export const pullRequestMark = (key: string) => `<!-- coder: ${key} -->`;

/** The coder of a pull request, by the mark in its description. */
const markedIn = (pullRequest?: { body?: string | null }) =>
  /<!-- coder: ([\w.-]+) -->/.exec(pullRequest?.body ?? '')?.[1];

/**
 * The coder an event is for (its key, `ENG-1.parser`), or undefined to drop
 * it: a person's new comment on a coder's pull request, or review of it; or
 * a CI run on its branch that failed.
 */
export function coderOf({ action, comment, review, issue, pull_request, workflow_run }: WebhookEvent) {
  const pullRequest = pull_request ?? (issue?.pull_request ? issue : undefined); // a comment's issue may be a pull request's
  if (comment && action === 'created' && !byAnAgent(comment.body ?? '')) return markedIn(pullRequest);
  // A review with no text, only comments on lines, comes as those comments too.
  if (
    review &&
    action === 'submitted' &&
    (review.body || review.state !== 'commented') &&
    !byAnAgent(review.body ?? '')
  )
    return markedIn(pullRequest);
  if (workflow_run && action === 'completed' && workflow_run.conclusion === 'failure')
    return coderOfBranch(workflow_run.head_branch);
  return undefined;
}

/**
 * What a coder reads of an event: what happened, on which pull request, and
 * who did it; not the rest of the event (GitHub sends the whole repository).
 */
export function whatHappened({ comment, review, issue, pull_request, workflow_run, sender }: WebhookEvent) {
  const pr = pull_request ?? issue ?? workflow_run?.pull_requests?.[0];
  const pullRequest = pr && { number: pr.number, title: pr.title, url: pr.html_url };
  const by = sender?.login;
  if (review) return { event: 'a review', pullRequest, by, state: review.state, body: review.body };
  if (comment?.path) {
    const { body, path, line, diff_hunk } = comment;
    return { event: 'a comment on a line', pullRequest, by, body, file: path, line, code: diff_hunk };
  }
  if (comment) return { event: 'a comment', pullRequest, by, body: comment.body };
  const { name, head_branch, html_url } = workflow_run;
  return { event: 'a CI run that failed', pullRequest, workflow: name, branch: head_branch, url: html_url };
}
