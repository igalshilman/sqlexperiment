/**
 * A team of agents on different frameworks: an editor (pi) hands research to
 * a researcher (LangGraph) and drafting to a writer (Claude), and puts the
 * piece together.
 *
 * Each member is an agent of its own, keyed by the editor's key and the task
 * (`researcher/alice.lisbon`), with its own conversation: the editor can come
 * back to a task, and a crash anywhere resumes where it stopped.
 *
 *   ANTHROPIC_API_KEY=… node team.ts
 *   npx @restatedev/restate-server   # Restate, in another terminal
 *   npx @restatedev/restate deployments register http://localhost:9080
 *   curl localhost:8080/editor/alice/chat -d 'Write a short guide to Lisbon for a first visit.'
 *   curl localhost:8080/writer/alice.guide/messages   # what the editor asked the writer (the task's name may differ)
 */
import { serve, subagent } from 'restate-agents';
import * as claude from 'restate-agents/claude';
import * as langgraph from 'restate-agents/langgraph';
import * as pi from 'restate-agents/pi';

const researcher = langgraph.agent({
  name: 'researcher',
  system: 'You research a topic and answer with the facts that matter, as a short list.',
});

const writer = claude.agent({
  name: 'writer',
  system: 'You write clear, warm prose from the notes you are given. Keep it short.',
});

const editor = pi.agent({
  name: 'editor',
  system: 'You edit a small team: research with the researcher, draft with the writer, then answer with the piece.',
  tools: [
    subagent(researcher, { description: 'a researcher, who answers with facts' }),
    subagent(writer, { description: 'a writer, who drafts prose from notes' }),
  ],
});

await serve(editor, researcher, writer);
