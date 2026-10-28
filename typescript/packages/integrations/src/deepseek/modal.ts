// Models on Modal's endpoints (modal.com/docs/guide/endpoints), for the
// DeepSeek Harness: the `modal` provider, its model ids those of the endpoint
// (`modal/deepseek-ai/DeepSeek-V4.1-Flash`). Its URL is MODAL_ENDPOINT_URL;
// it is called with a proxy token's MODAL_PROXY_KEY and MODAL_PROXY_SECRET.
//
// The endpoint speaks Anthropic's Messages API, as DeepSeek's does: the
// harness's DeepSeek transport (@deepseek-ai/dsh-llm-deepseek) calls it, with
// the token and the endpoint's routing affinity in place of DeepSeek's key.
import type { Context } from '@deepseek-ai/cordis';
import { LlmError } from '@deepseek-ai/dsh-llm';
import { catalogModelInfo, registerDeepSeekProvider, resolveAdapterOptions } from '@deepseek-ai/dsh-llm-deepseek';

export const name = 'llm-modal';
export const inject = ['llm'];

/** The provider's name, in `modal/<model-id>`. */
export const PROVIDER = 'modal';

export interface Config {
  /** The endpoint's model id, such as deepseek-ai/DeepSeek-V4.1-Flash. */
  model: string;
  /** The conversation, which the endpoint keeps on one container: its prompt cache is reused. */
  affinity: string;
}

/** `variable` of the environment, or an error that says to set it. */
function required(variable: string): string {
  const value = process.env[variable];
  if (!value) throw new LlmError(`modal: set ${variable}, for the models on Modal's endpoint`, 'MISSING_CREDENTIAL');
  return value;
}

export function apply(ctx: Context, { model, affinity }: Config) {
  const options = () =>
    resolveAdapterOptions({
      baseURL: required('MODAL_ENDPOINT_URL'),
      models: [{ id: model, contextWindow: 1_000_000 }],
    });
  registerDeepSeekProvider(ctx, PROVIDER, {
    options,
    providerName: 'Modal',
    resolveAuth: async () => ({
      headers: {
        authorization: `Bearer ${required('MODAL_PROXY_KEY')}.${required('MODAL_PROXY_SECRET')}`,
        'modal-routing-affinity-key': affinity,
      },
    }),
    discoverModels: async (provider) => options().models.map((m) => catalogModelInfo(provider, m)),
  });
}
