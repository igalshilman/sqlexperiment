// Models on Modal's endpoints (modal.com/docs/guide/endpoints), for the
// DeepSeek Harness: the `modal` provider, its model ids those of the endpoint
// (`modal/deepseek-ai/DeepSeek-V4.1-Flash`). Its URL is MODAL_ENDPOINT_URL;
// it is called with a proxy token's MODAL_PROXY_KEY and MODAL_PROXY_SECRET.
//
// The endpoint is an OpenAI-compatible gateway, not DeepSeek's API: the
// harness reaches it as a route of its pi-ai adapter (@deepseek-ai/dsh-llm-pi-ai),
// Chat Completions with `reasoning_effort`. DeepSeek's own transport
// (@deepseek-ai/dsh-llm-deepseek) does not fit it: it asks for Messages with
// `thinking: { type: 'enabled' }` and no `budget_tokens`, which DeepSeek's API
// takes and the endpoint refuses (400).
import { LlmError } from '@deepseek-ai/dsh-llm';

/** The provider's name, in `modal/<model-id>`. */
export const PROVIDER = 'modal';

/** `variable` of the environment, or an error that says to set it. */
function required(variable: string): string {
  const value = process.env[variable];
  if (!value) throw new LlmError(`modal: set ${variable}, for the models on Modal's endpoint`, 'MISSING_CREDENTIAL');
  return value;
}

/**
 * pi-ai's providers for `model` on the endpoint: the `modal` route, for the
 * conversation `affinity`, which the endpoint keeps on one container: its
 * prompt cache is reused. The proxy token is the route's `Authorization`
 * header (the route names no key).
 */
export function providers(model: string, affinity: string) {
  const route = {
    displayName: 'Modal',
    api: 'openai-completions' as const,
    baseURL: `${required('MODAL_ENDPOINT_URL').replace(/\/+$/, '')}/v1`,
    headers: {
      authorization: `Bearer ${required('MODAL_PROXY_KEY')}.${required('MODAL_PROXY_SECRET')}`,
      'modal-routing-affinity-key': affinity,
    },
    // pi-ai takes an endpoint it does not know for OpenAI's, and sends the
    // system prompt as a `developer` message: the endpoint drops those
    // without a word (the model never sees the prompt). `system` it keeps.
    compat: { supportsDeveloperRole: false },
    // DeepSeek's efforts, `high` unless the request picks another.
    reasoning: 'high' as const,
    models: [{ id: model, contextWindow: 1_000_000, reasoningEfforts: { low: 'low', high: 'high', max: 'max' } }],
  };
  return { [PROVIDER]: route };
}
