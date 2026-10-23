// The gateway's parts without a Restate server: the prebuilt tools, the
// prompt and the workspaces. Against a server: demo.sh.
import { describe, expect, it } from 'vitest';
import { z } from 'zod';
import { EXAMPLES, PROMPT_SCHEMA, workspaceOf } from '../../examples-typescript/advanced/gateway/agent.ts';
import { identity } from '../../examples-typescript/advanced/gateway/gateway.ts';
import { PREBUILT } from '../../examples-typescript/advanced/gateway/tools.ts';

describe('the gateway', () => {
  it('has prebuilt tools, with the JSON schema of their zod input', () => {
    expect(Object.keys(PREBUILT)).toEqual(['lookup_order', 'refund', 'open_ticket']);
    expect(PREBUILT.refund).toMatchObject({ approval: true, service: 'Payments', handler: 'refund' });
    expect(PREBUILT.lookup_order!.inputSchema).toEqual({
      type: 'object',
      properties: { order: { type: 'string', description: 'The order id, such as "101"' } },
      required: ['order'],
      additionalProperties: false,
    });
  });

  it('takes the tenant and the user from `<tenant>.<user>`', () => {
    expect(identity('acme.alice')).toEqual({ tenant: 'acme', user: 'alice' });
    expect(identity('acme')).toEqual({ tenant: 'acme', user: 'anonymous' });
  });
});

describe('the prompt', () => {
  it('takes a string, and suggests the first example', () => {
    // An input the SDK announces: an empty body is not one.
    expect(PROMPT_SCHEMA['~standard'].validate(undefined)).toHaveProperty('issues');
    expect(z.toJSONSchema(PROMPT_SCHEMA)).toMatchObject({ type: 'string', default: EXAMPLES[0], examples: EXAMPLES });
  });
});

describe('the workspace', () => {
  it('is a directory per object, only with WORKSPACE', () => {
    expect(workspaceOf('Assistant', 'acme.alice', '/w')).toBe('/w/Assistant/acme.alice');
    expect(workspaceOf('Case', '../../etc', '/w')).toBe('/w/Case/.._.._etc');
    expect(workspaceOf('Case', '..', '/w')).toBe('/w/Case/_..');
    expect(workspaceOf('Case', 'acme.103', '')).toBeUndefined();
  });
});
