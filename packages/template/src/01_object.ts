/**
 * 01 · A virtual object
 *
 * Throughout this tutorial we build a wallet. Every user has one, and Restate
 * finds it by key: `wallet/alice` and `wallet/bob` are two instances of the
 * same object. Restate runs the requests for one key one at a time, so a
 * wallet never handles two of them at once.
 *
 * This process serves the wallet's handlers. Restate calls them over HTTP,
 * and you call them through Restate:
 *
 *   node src/01_object.ts                                                           # serves the wallet on port 9080
 *   npx @restatedev/restate-server                                                  # Restate, in another terminal
 *   npx @restatedev/restate -y deployments register --force http://localhost:9080   # tells Restate where the wallet is
 *   curl localhost:8080/wallet/alice/hello                                          # "Hello, alice!"
 *
 * Every lesson serves on port 9080, so stop this one before you start the
 * next, and register again.
 */
import * as restate from 'restate-sessions';

// A session object: a virtual object that gets a database in the next lesson.
const wallet = restate.sessionObject({
  name: 'wallet',
  handlers: {
    // `ctx.key` is the key the request was sent to: the wallet's owner.
    hello: async (ctx) => `Hello, ${ctx.key}!`,
  },
});

await restate.serve({ services: [wallet], port: 9080 });
