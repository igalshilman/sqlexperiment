// Test only: pi-ai's providers, and the teller among them as the provider
// `teller` (pi.ts), for MODEL=teller/teller: what an agent's import of
// @earendil-works/pi-ai/providers/all gets in place of pi-ai's (see fakes.ts).
import { builtinProviders as builtin } from '@earendil-works/pi-ai/providers/all';
import { tellerProvider } from './pi.ts';

export * from '@earendil-works/pi-ai/providers/all';

/** pi-ai's providers, and the teller. */
export const builtinProviders = () => [...builtin(), tellerProvider()];
