// Test only: restate-agents/opencode/db, whose OpenCode has the teller (see opencode.ts
// and fakes.ts).
import { openOpenCode as open } from 'restate-agents/opencode/db';
import { withTeller } from './opencode.ts';

export * from 'restate-agents/opencode/db';
export const openOpenCode = withTeller(open);
