/**
 * 10 · An agent
 *
 * An agent is an object that holds a conversation: its handler `chat` answers
 * a message, and its view `messages` reads the conversation so far (other
 * views, such as `status` and `stop`, follow and steer it: see
 * restate-agents' README). Each key is a conversation of its own, in its own
 * database, answered one message at a time.
 *
 *   OPENAI_API_KEY=… node src/10_agent.ts          # and register it (see 01)
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
