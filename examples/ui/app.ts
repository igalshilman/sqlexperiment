// A chat page for the support agent of ../support.ts, on Restate's own
// client: send a message, see what the agent is doing while it works,
// approve or reject what it waits for, steer it, or stop it.
//
//   ANTHROPIC_API_KEY=… node support.ts                                 # the agent, on port 9080
//   npx @restatedev/restate-server                                       # Restate, in another terminal
//   npx @restatedev/restate deployments register http://localhost:9080
//   npx vite ui                                                          # this page: http://localhost:5173
//
// Each user (`?user=alice`) is a conversation of its own: the agent's key.
import * as clients from '@restatedev/restate-sdk-clients';
import { agent, type ChatMessage, type Part, type Status } from 'restate-agents/client';

// Restate's ingress, through the dev server's proxy (vite.config.ts).
const restate = clients.connect({ url: `${location.origin}/ingress` });
const user = new URLSearchParams(location.search).get('user') ?? 'alice';
const support = restate.objectClient(agent('support'), user);

const $ = (id: string) => document.getElementById(id)!;
const element = (tag: string, className: string, text = '') => {
  const e = document.createElement(tag);
  e.className = className;
  e.textContent = text;
  return e;
};

/**
 * Send `text`: a message of its own, or, while the agent answers one, added
 * to that one (`steer`): the agent takes it at its next tool call.
 */
async function send(text: string) {
  const { answering } = await support.status();
  if (answering) await support.steer(text);
  else await restate.objectSendClient(agent('support'), user).chat(text);
  await refresh();
}

/** A tool call: its name and input, and its output once it ran. */
function tool(part: Extract<Part, { type: 'tool' }>) {
  const e = element('div', part.error ? 'tool error' : 'tool', `${part.name}(${JSON.stringify(part.input)})`);
  e.append(element('div', 'output', part.output ?? '…'));
  return e;
}

function renderMessages(messages: ChatMessage[]) {
  const list = $('messages');
  list.replaceChildren(
    ...messages.map((m) => {
      const e = element('div', `message ${m.role}`);
      for (const part of m.parts) e.append(part.type === 'text' ? element('p', 'text', part.text) : tool(part));
      return e;
    }),
  );
  list.scrollTop = list.scrollHeight;
}

/** What the agent is doing: the message it answers (with Stop), and what its tools wait for (with Approve and Reject). */
function renderStatus({ answering, waiting }: Status) {
  $('working').hidden = answering === null;
  $('send').textContent = answering ? 'Steer' : 'Send';
  $('waiting').replaceChildren(
    ...waiting.map((w) => {
      const e = element('div', 'wait', `Waits for ${w.name}: ${JSON.stringify(w.input)}`);
      for (const value of ['approved', 'rejected']) {
        const button = element('button', value, value === 'approved' ? 'Approve' : 'Reject');
        button.addEventListener('click', () => void support.signal({ id: w.id, value }).then(refresh));
        e.append(button);
      }
      return e;
    }),
  );
}

async function refresh() {
  const [status, { messages }] = await Promise.all([support.status(), support.messages({ limit: 50 })]);
  renderStatus(status);
  renderMessages(messages);
}

$('user').textContent = user;
$('stop').addEventListener('click', () => void support.stop().then(refresh));
$('composer').addEventListener('submit', (event) => {
  event.preventDefault();
  const input = $('text') as HTMLInputElement;
  if (input.value.trim()) void send(input.value);
  input.value = '';
});
// The conversation and the agent's status, every second: the tool calls
// show up as it makes them.
for (;;) {
  await refresh().catch((error: unknown) => console.error(error));
  await new Promise((resolve) => setTimeout(resolve, 1000));
}
