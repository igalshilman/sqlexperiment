/**
 * 15 · Agents that delegate
 *
 * `subagent(agent)` makes another agent a tool: the model hands it a named
 * task, and gets its answer. Each task is a conversation of the other agent,
 * keyed by this agent's key and the task's name (such as
 * `researcher/alice.lisbon`), so the model can come back to it, and a crash
 * anywhere resumes where it stopped.
 *
 *   node src/15_subagents.ts                    # and register it (see 01)
 *   curl localhost:8080/editor/alice/chat -d 'Write a short guide to Lisbon for a first visit.'
 *   curl localhost:8080/editor/alice/messages   # the tasks it handed out, and the guide
 */
import { serve, subagent } from 'restate-agents';
import * as pi from 'restate-agents/pi';

const researcher = pi.agent({
  name: 'researcher',
  description: 'Researches a topic, and answers with the facts that matter',
  system: 'You research a topic, and answer with the facts that matter, as a short list.',
});

const editor = pi.agent({
  name: 'editor',
  system: 'You write short, warm guides. Ask the researcher for the facts first.',
  tools: [subagent(researcher)],
});

await serve(editor, researcher);
