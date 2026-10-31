/**
 * 01 · A virtual object
 *
 * A virtual object is a set of handlers that Restate calls by key: each key
 * (here, a name) is an instance of its own, and Restate runs its calls one at
 * a time. `restate.sessionObject` makes one; each handler gets its context,
 * `ctx`, with the key. This process serves the handlers; Restate calls them over HTTP, and
 * you call them through Restate.
 *
 *   node src/01_greeter.ts                                                          # the object, on port 9080
 *   npx @restatedev/restate-server                                                  # Restate, in another terminal
 *   npx @restatedev/restate -y deployments register --force http://localhost:9080   # tell Restate where it is
 *   curl localhost:8080/greeter/alice/greet                                         # "Hello, alice!"
 *
 * Every lesson serves on port 9080: register it again when you start the next.
 */
import * as restate from 'restate-sessions';

const greeter = restate.sessionObject({
  name: 'greeter',
  handlers: {
    greet: async (ctx) => `Hello, ${ctx.key}!`,
  },
});

await restate.serve({ services: [greeter], port: 9080 });
