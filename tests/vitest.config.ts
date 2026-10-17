import { defineConfig } from 'vitest/config';

// LangGraph's checkpointer suite (storage/langgraph.test.ts) uses the
// test globals.
export default defineConfig({ test: { globals: true } });
