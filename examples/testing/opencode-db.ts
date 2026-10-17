// Test only: opencode-db.ts, whose OpenCode has the teller (see opencode.ts
// and fakes.ts).
import { openOpenCode as open } from '../opencode/opencode-db.ts';
import { withTeller } from './opencode.ts';

export * from '../opencode/opencode-db.ts';
export const openOpenCode = withTeller(open);
