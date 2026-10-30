// Test only: the scripted tellers in place of the real models, for the
// examples run as processes, such as the crash test's (../test.ts):
//
//   MODEL=teller/teller node --import ./fakes/fakes.ts serve.ts
//
// An example's import of a model's module gets the teller's module instead
// (the unit tests do the same with vi.mock):
// - pi-ai's provider `teller`: pi.ts;
// - @anthropic-ai/sdk, of the Claude agent: claude.ts;
// - @langchain/openai and @langchain/anthropic: langgraph.ts;
// - DeepSeek's provider plugin: deepseek.ts;
// - restate-agents/opencode/db, whose OpenCode gets the teller's plugin:
//   opencode-db.ts.
// Imports of the tellers' modules and of packages are left as they are.
import { registerHooks } from 'node:module';

const here = new URL('./', import.meta.url).href;
const fake = (path: string) => new URL(path, import.meta.url).href;
/** The fakes of packages, by specifier. */
const packages: Record<string, string> = {
  '@earendil-works/pi-ai/providers/teller': fake('pi.ts'),
  '@anthropic-ai/sdk': fake('claude.ts'),
  '@langchain/openai': fake('langgraph.ts'),
  '@langchain/anthropic': fake('langgraph.ts'),
  '@deepseek-ai/dsh-llm-deepseek-api-key': fake('deepseek.ts'),
};
/** The fakes of the examples' modules, by URL. */
const modules: Record<string, string> = {
  [fake('../../agents/src/opencode/db.ts')]: fake('opencode-db.ts'),
};

registerHooks({
  resolve(specifier, context, next) {
    const parent = context.parentURL ?? '';
    if (parent.startsWith(here) || parent.includes('/node_modules/')) return next(specifier, context);
    const url = packages[specifier];
    if (url) return { url, format: 'module-typescript', shortCircuit: true };
    const resolved = next(specifier, context);
    const module = modules[resolved.url];
    return module ? { ...resolved, url: module } : resolved;
  },
});
