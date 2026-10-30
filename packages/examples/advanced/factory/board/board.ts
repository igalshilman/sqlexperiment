// The factory's board: each issue as it is worked on, live. Triage, the
// planner, and a lane per part of the plan, with its coder and reviewer;
// each agent glows while it works, and shows what it does. Click an agent
// for its conversation. For a demo: it reads the agents' conversations
// through Restate's client, and changes nothing.
//
//   node advanced/factory/factory.ts           # the factory (README.md), with Restate
//   npx vite advanced/factory/board            # this page: http://localhost:5173
//
// What the board shows, it reads from triage's conversation: its calls to
// the planner, the coders and the reviewer (the `subagent` tools), with the
// task names that key them (`coder/ENG-1.parser`), and their answers (a
// `Plan`, a `Built`, a `Review`). Each agent it finds there, it follows.
import * as clients from '@restatedev/restate-sdk-clients';
import { agent, type ChatMessage, follow, type Part, type Status } from 'restate-agents/client';

// Restate's ingress and its admin API (for its SQL), through the dev
// server's proxy (vite.config.ts).
const restate = clients.connect({ url: `${location.origin}/ingress` });

type Role = 'triage' | 'planner' | 'coder' | 'reviewer';
type Tool = Extract<Part, { type: 'tool' }>;

const roles: Record<Role, string> = {
  triage: '🧭 Triage',
  planner: '📐 Planner',
  coder: '🛠️ Coder',
  reviewer: '🔍 Reviewer',
};

/** An agent of the issue, as followed: what it does now, and its conversation. */
interface Followed {
  role: Role;
  key: string;
  status: Status | null;
  messages: Map<number, ChatMessage>;
  /** What came to it from outside, such as a comment through GitHub's webhook, while the board watched. */
  news?: { text: string; at: number };
}

/** The agents followed, by `role/key`. */
const followed = new Map<string, Followed>();
let issue = new URLSearchParams(location.search).get('issue') ?? '';
/** The agent whose conversation is open, if one is. */
let open: Followed | undefined;

/** Follow the agent `role/key`, from now on: each change re-renders the board. */
function followAgent(role: Role, key: string) {
  const id = `${role}/${key}`;
  let agentFollowed = followed.get(id);
  if (agentFollowed) return agentFollowed;
  agentFollowed = { role, key, status: null, messages: new Map() };
  followed.set(id, agentFollowed);
  void (async (a: Followed) => {
    let loaded = false;
    for await (const change of follow(restate, agent(role), key)) {
      for (const m of change.messages) {
        // A message from outside, after what was there when the board opened: flash it.
        const news = loaded && m.role === 'user' && !a.messages.has(m.id) && webhookOf(m);
        if (news) a.news = { text: news, at: Date.now() };
        a.messages.set(m.id, m);
      }
      for (const old of a.messages.keys()) if (old < change.first) a.messages.delete(old);
      a.status = change.status;
      loaded = true;
      schedule();
    }
  })(agentFollowed);
  return agentFollowed;
}

const sorted = (a: Followed) => [...a.messages.values()].toSorted((x, y) => x.id - y.id);
const tools = (a: Followed) => sorted(a).flatMap((m) => m.parts.filter((p): p is Tool => p.type === 'tool'));
/** The task a `subagent` call names: `parser`, of `coder/ENG-1.parser`. */
const taskOf = (call: Tool) => (call.input as { name: string }).name;
const text = (m: ChatMessage) => m.parts.map((p) => (p.type === 'text' ? p.text : '')).join('');

/** The event of a `<webhook>` message, as much of it as the board reads: Linear's, or GitHub's (github.ts). */
interface WebhookEvent {
  type?: string;
  action?: string;
  data?: { title?: string; url?: string };
  event?: string;
  by?: string;
}

/** The event a `<webhook>` message carries, or undefined for another message. */
function eventOf(m: ChatMessage): WebhookEvent | undefined {
  const match = /^<webhook name="[^"]*"[^>]*>\n([\s\S]*)\n<\/webhook>$/.exec(text(m).trim());
  if (!match) return undefined;
  try {
    return JSON.parse(match[1]!) as WebhookEvent;
  } catch {
    return undefined;
  }
}

/** What a webhook message says happened, in a line: `🔔 a comment by alice`. */
function webhookOf(m: ChatMessage) {
  const event = eventOf(m);
  if (!event) return undefined;
  if (event.event) return `🔔 ${event.event}${event.by ? ` by ${event.by}` : ''}`; // GitHub's (github.ts)
  return `🔔 ${event.type ?? 'an event'} ${event.action ?? ''}`.trim(); // Linear's
}

/** An agent's answer (its `output`), parsed, or undefined until it has one. */
function answerOf(call: Tool | undefined): unknown {
  if (!call?.output || call.error) return undefined;
  try {
    return JSON.parse(call.output);
  } catch {
    return undefined;
  }
}

interface Plan {
  summary: string;
  parts: { name: string; title: string }[];
}
interface Built {
  pullRequest: string;
  number: number;
  summary: string;
}
interface Review {
  approved: boolean;
  summary: string;
}

/** A tool call, in a line: its name, and what it is about (`bash: pytest -q`). */
function describe(call: Tool) {
  const input = call.input as Record<string, unknown>;
  const what = [input.command, input.filePath, input.path, input.name, input.title].find((v) => typeof v === 'string');
  return what ? `${call.name}: ${what}` : call.name;
}

/** What an agent does now, in a line: its tool call that runs, or what it said last. */
function doing(a: Followed): string {
  if (!a.status?.answering) return '';
  const running = tools(a).findLast((t) => t.output === undefined);
  if (running) return describe(running);
  const said = sorted(a).findLast((m) => m.role === 'assistant' && text(m).trim());
  return said ? text(said).trim().split('\n')[0]! : 'thinking…';
}

// ---- Rendering

const $ = (id: string) => document.getElementById(id)!;
const element = (tag: string, className = '', content = '') => {
  const e = document.createElement(tag);
  e.className = className;
  e.textContent = content;
  return e;
};
const link = (href: string, content: string) => {
  const a = element('a', 'link', content) as HTMLAnchorElement;
  a.href = href;
  a.target = '_blank';
  return a;
};

/** A card for the agent `a` (or for one not called yet): it glows while it works. */
function card(role: Role, a: Followed | undefined, ...extra: (Node | string)[]) {
  const working = Boolean(a?.status?.answering);
  const e = element(
    'button',
    `card ${role}${working ? ' working' : ''}${a ? '' : ' idle'}${a && a === open ? ' open' : ''}`,
  );
  e.append(element('div', 'role', roles[role]), element('div', 'key', a?.key ?? 'not yet'));
  const now = a && doing(a);
  if (now) e.append(element('div', 'doing', now));
  e.append(...extra);
  if (a?.news && Date.now() - a.news.at < 8000) e.append(element('div', 'news', a.news.text));
  if (a) e.addEventListener('click', () => openConversation(a));
  return e;
}

/** The line between two cards, flowing while work passes along it, with its label above. */
function edge(active: boolean, label = '') {
  const e = element('div', `edge${active ? ' active' : ''}`);
  e.append(element('span', '', label));
  return e;
}

/** The panel's foldouts open, by message and part, so that they stay open as it renders again. */
const unfolded = new Set<string>();
function foldout(id: string, className: string, summary: string, ...content: string[]) {
  const e = element('details', className) as HTMLDetailsElement;
  e.open = unfolded.has(id);
  e.addEventListener('toggle', () => (e.open ? unfolded.add(id) : unfolded.delete(id)));
  e.append(element('summary', '', summary), ...content.filter(Boolean).map((c) => element('pre', '', c)));
  return e;
}

function renderIssue() {
  const board = $('board');
  if (!issue) {
    board.replaceChildren(element('p', 'empty', 'No issue yet: create one in Linear, and it shows up here.'));
    return;
  }
  const triage = followAgent('triage', issue);
  const calls = tools(triage);
  const subagent = (role: Role) => calls.filter((t) => t.name === role);

  // The issue, as Linear's webhook brought it.
  const created = sorted(triage)
    .map(eventOf)
    .find((e) => e?.type === 'Issue')?.data;
  const head = element('div', 'issue');
  head.append(element('div', 'id', issue), element('div', 'title', created?.title ?? ''));
  if (created?.url) head.append(link(created.url, 'Linear ↗'));

  const planCall = subagent('planner').at(-1);
  const planner = planCall && followAgent('planner', `${issue}.${taskOf(planCall)}`);
  const plan = answerOf(planCall) as Plan | undefined;
  const top = element('div', 'top');
  top.append(
    head,
    edge(Boolean(triage.status?.answering)),
    card('triage', triage),
    edge(Boolean(planner?.status?.answering), plan ? `${plan.parts.length} parts` : ''),
    card('planner', planner, plan ? element('div', 'summary', plan.summary) : ''),
  );

  // A lane for each part: of the plan, or that triage gave a coder.
  const names = [...new Set([...(plan?.parts.map((p) => p.name) ?? []), ...subagent('coder').map(taskOf)])];
  const lanes = names.map((name) => {
    const title = plan?.parts.find((p) => p.name === name)?.title ?? name;
    const coderCalls = subagent('coder').filter((t) => taskOf(t) === name);
    const reviews = subagent('reviewer').filter((t) => taskOf(t) === name);
    const coder = coderCalls.length > 0 ? followAgent('coder', `${issue}.${name}`) : undefined;
    const reviewer = reviews.length > 0 ? followAgent('reviewer', `${issue}.${name}`) : undefined;
    const built = coderCalls.map((t) => answerOf(t) as Built | undefined).findLast(Boolean);
    const review = answerOf(reviews.at(-1)) as Review | undefined;

    const lane = element('div', `lane${review?.approved ? ' approved' : ''}`);
    lane.append(element('div', 'part', title));
    lane.append(card('coder', coder, built ? link(built.pullRequest, `PR #${built.number} ↗`) : ''));
    const handing = Boolean(coder?.status?.answering || reviewer?.status?.answering);
    lane.append(edge(handing, reviews.length > 0 ? `round ${reviews.length}` : ''));
    const verdict = review && element('div', 'verdict', review.approved ? '✅ Approved' : '🔁 Changes asked for');
    lane.append(card('reviewer', reviewer, verdict ?? ''));
    return lane;
  });
  const parts = element('div', 'lanes');
  parts.append(...lanes);
  board.replaceChildren(top, parts);
}

/** The open agent's conversation: its messages, its tool calls with their outputs, and what came to it from outside. */
function renderConversation() {
  $('panel').hidden = !open;
  if (!open) return;
  $('panel-title').textContent = `${roles[open.role]} · ${open.key}`;
  const list = $('messages');
  const atBottom = list.scrollHeight - list.scrollTop - list.clientHeight < 40;
  list.replaceChildren(
    ...sorted(open).map((m) => {
      const e = element('div', `message ${m.role}`);
      const news = webhookOf(m);
      const id = `${open!.role}/${open!.key}/${m.id}`;
      if (news) {
        e.append(foldout(id, 'webhook', news, JSON.stringify(eventOf(m), null, 2)));
        return e;
      }
      for (const [i, part] of m.parts.entries()) {
        if (part.type === 'text') e.append(element('p', 'text', part.text));
        else {
          const summary = `${describe(part)}${part.output === undefined ? ' …' : ''}`;
          const input = JSON.stringify(part.input, null, 2);
          e.append(foldout(`${id}/${i}`, part.error ? 'tool error' : 'tool', summary, input, part.output ?? ''));
        }
      }
      return e;
    }),
  );
  if (atBottom) list.scrollTop = list.scrollHeight;
}

function openConversation(a: Followed) {
  open = a;
  schedule();
}

let scheduled = false;
/** Render once on the next frame, for all the changes until then. */
function schedule() {
  if (scheduled) return;
  scheduled = true;
  requestAnimationFrame(() => {
    scheduled = false;
    renderIssue();
    renderConversation();
  });
}

// ---- The issues

/** The number of an issue, `1` of `ENG-1`: the newest issues first. */
const issueNumber = (key: string) => Number(/\d+$/.exec(key)?.[0] ?? 0);

/** The issues triage has, from Restate's SQL over its state: one each. */
async function issues(): Promise<string[]> {
  const query = "SELECT DISTINCT service_key FROM state WHERE service_name = 'triage'";
  const response = await fetch('/admin/query', {
    method: 'POST',
    headers: { 'content-type': 'application/json', accept: 'application/json' },
    body: JSON.stringify({ query }),
  });
  const { rows } = (await response.json()) as { rows: { service_key: string }[] };
  return rows.map((r) => r.service_key).toSorted((a, b) => issueNumber(b) - issueNumber(a) || a.localeCompare(b));
}

function select(key: string) {
  issue = key;
  open = undefined;
  history.replaceState(null, '', `?issue=${encodeURIComponent(key)}`);
  schedule();
}

async function renderIssues() {
  const keys = await issues().catch(() => []);
  if (!issue && keys[0]) select(keys[0]);
  $('issues').replaceChildren(
    ...keys.map((key) => {
      const working = followed.get(`triage/${key}`)?.status?.answering;
      const e = element('button', `issue-item${key === issue ? ' selected' : ''}${working ? ' working' : ''}`, key);
      e.addEventListener('click', () => select(key));
      return e;
    }),
  );
}

$('close').addEventListener('click', () => {
  open = undefined;
  schedule();
});
schedule();
for (;;) {
  await renderIssues();
  schedule(); // the news fades
  await new Promise((resolve) => setTimeout(resolve, 2000));
}
