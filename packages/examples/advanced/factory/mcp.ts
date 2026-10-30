/**
 * An MCP server's tools, as tools for any agent: each of its tools, with the
 * input its JSON schema describes, called on the server when the model calls
 * it. An error the server answers with is the tool's answer to the model.
 *
 * A tool call is stored once it is done, with the agent's conversation: a
 * crash makes again only a call that had not finished, which the server sees
 * twice (moving an issue twice is harmless; a comment may show twice).
 */
import { Client, StreamableHTTPClientTransport } from '@modelcontextprotocol/client';
import * as restate from '@restatedev/restate-sdk';
import { type Tool, tool } from 'restate-agents';
import { z } from 'zod';

/** The tools of the MCP server at `url`, asked for once, when the endpoint starts. */
export async function mcpTools(url: string, headers: Record<string, string> = {}): Promise<Tool[]> {
  const connect = async () => {
    const client = new Client({ name: 'factory', version: '1.0.0' });
    await client.connect(new StreamableHTTPClientTransport(new URL(url), { requestInit: { headers } }));
    return client;
  };
  let client = await connect();
  const { tools } = await client.listTools();

  /** A tool call, on a new connection if the one before is gone (the server restarted, say). */
  const call = async (name: string, input: Record<string, unknown>) => {
    try {
      return await client.callTool({ name, arguments: input });
    } catch {
      client = await connect();
      return client.callTool({ name, arguments: input });
    }
  };

  return tools.map((t) =>
    tool({
      name: t.name,
      description: t.description ?? t.name,
      input: z.fromJSONSchema(t.inputSchema as z.core.JSONSchema.JSONSchema) as z.ZodObject,
      run: async (input) => {
        const result = await call(t.name, input);
        const text = result.content.map((part) => (part.type === 'text' ? part.text : `[${part.type}]`)).join('\n');
        if (result.isError) throw new restate.TerminalError(text);
        return text;
      },
    }),
  );
}
