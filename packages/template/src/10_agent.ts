/**
 * 10 · An agent
 *
 * An agent is an object whose handlers are a conversation: `chat` answers a
 * message, `messages` reads the conversation so far (and `status`, `stop`
 * and more: see restate-agents' README). Each key is a conversation of its
 * own, in its own database, answered one message at a time.
 *
 *   node src/10_agent.ts                           # and register it (see 01)
 *   curl localhost:8080/assistant/alice/chat -d 'Hi, I am Alice.'
 *   curl localhost:8080/assistant/alice/chat -d 'What is my name?'
 *   curl localhost:8080/assistant/alice/messages   # the conversation so far
 */
import { serve } from 'restate-agents';
import * as pi from 'restate-agents/pi';

const assistant = pi.agent({
  name: 'assistant',
  system: 'You are a friendly assistant. Keep your answers short.',
});

await serve(assistant);
