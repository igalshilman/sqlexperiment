import { defineConfig } from 'vitest/config';

// LangGraph's checkpointer suite (langgraph/checkpointer.test.mjs) uses the
// test globals.
export default defineConfig({ test: { globals: true } });
