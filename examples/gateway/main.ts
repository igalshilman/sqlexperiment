// The tool gateway's endpoint: everything, or a part, `tools` (the tools and
// the Gateway: stateless, so anywhere) or `agents` (the Assistant, keyed per
// user, and the Case, keyed per order: in a sandbox).
//
//   PI_MODEL=openai/gpt-6-luna OPENAI_API_KEY=… node gateway/main.ts [tools|agents]
import { serve } from 'restate-agents';
import { assistant, caseAgent } from './agent.ts';
import { gateway } from './gateway.ts';
import { tools } from './tools.ts';

const parts = { tools: [...tools, gateway], agents: [assistant, caseAgent] };
const part = process.argv[2] as keyof typeof parts | undefined;
await serve(...(part ? parts[part] : [...parts.tools, ...parts.agents]));
