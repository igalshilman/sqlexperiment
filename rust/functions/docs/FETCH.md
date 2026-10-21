# fetch: programs' HTTP requests

Programs reach the outside world with `fetch`: the standard `fetch(url,
init)` in JavaScript, `await ctx.fetch(url, ...)` in Python. The host sends
each request for the program, under a policy the deployment sets. Models are
reached this way too: the host has no model code, only HTTP.

```js
const response = await fetch('https://api.anthropic.com/v1/messages', {
  method: 'POST',
  headers: { 'anthropic-version': '2023-06-01' },
  body: fetch.json(request), // or a string, or bytes
});
const message = await response.json();
```

```python
response = await ctx.fetch(url, method="POST", headers={...}, json=request)
message = response.json()
```

## How a request goes

1. The program calls `fetch`. It gets a promise, and keeps running until it
   waits.
2. The turn commits: the program's memory, with the request in it, and
   "waiting on `fetch` #7". The commit does not copy the request, and Restate
   never sees it.
3. The host reads the request from the program's memory and sends it.
4. The response is delivered to the program, as a `Response`, and saved with
   its memory at the next commit.

A crash between 3 and 4 sends the request again, exactly as it was: a
request is sent **at least once**. A program that must not repeat a request
sends its own `Idempotency-Key` header: a key it makes before the request
(`Math.random()` is fine) is in its memory, so it is the same on every
attempt (the test `a_request_is_sent_again_after_a_crash_as_it_was`).

`fetch.json(data)` (`json=` in Python) is a body the host serializes as it
sends it. With `JSON.stringify(data)` instead, the program would build the
whole request as one string, in its memory, at every call. For an agent that
string is the whole conversation, so this costs time and storage (the coding
session of DESIGN.md §10.1, at 636 KiB of conversation):

| Body                   | Turn before the model call | Stored over 200 steps |
| ---------------------- | -------------------------: | --------------------: |
| `JSON.stringify(data)` |                      16 ms |               3.9 MiB |
| `fetch.json(data)`     |                     1.1 ms |               3.2 MiB |

## The policy

A deployment says where programs may send requests, and what the host adds
to them:

```rust
let policy = FetchPolicy::new()
    .allow("https://api.example.com")
    .secret("https://api.anthropic.com", "x-api-key", &key) // allows it too
    .max_response(10 << 20)
    .timeout(Duration::from_secs(300));
Extensions::new().with("fetch", Fetch::new(policy))
```

`FetchPolicy::new()` allows nothing. The binary's policy
(`FetchPolicy::from_env`) allows the Claude API, adds ANTHROPIC_API_KEY to its
requests, and allows the origins listed in FUNCTIONS_FETCH_ALLOW
(comma-separated). `allow_public()` allows any public origin, and
`allow_private_networks()` lets allowed origins be on this machine or its
network, for local development.

## Security and isolation

| What a program might try                                                                   | What happens                                                                                                                                                               |
| ------------------------------------------------------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Call an origin the policy does not allow                                                   | Refused before anything is sent                                                                                                                                            |
| Reach it through a redirect from an allowed one                                            | Every hop is checked, and refused the same way                                                                                                                             |
| Reach the host's network: Restate's admin API, cloud metadata (169.254.169.254), localhost | Refused, by address in the URL and by what a name resolves to, even for an allowed name. Proxies from the environment are not used, so the check sees the real destination |
| Read the API key                                                                           | The host adds it to requests to its origin only. The program never has it, so it is not in its memory, nor in the state saved in Restate                                   |
| Replace the API key with its own                                                           | The host's value wins                                                                                                                                                      |
| Leak the key, or its own credentials, through a redirect                                   | The key is added per hop, by the hop's origin. The program's `authorization` and `cookie` go no further than their origin. No `Referer` is sent                            |
| See another run's cookies                                                                  | The host keeps no cookies, cache or credentials between requests. A program sees `set-cookie` and sends its own `cookie`, from its own memory                              |
| Set `host`, `content-length`, `transfer-encoding`, `proxy-*`...                            | Refused: those are the host's                                                                                                                                              |
| Use `file:`, `data:` or credentials in the URL; `TRACE`, `CONNECT`                         | Refused                                                                                                                                                                    |
| Exhaust memory or hold a turn open                                                         | Responses above `max_response` (10 MiB) and requests over `timeout` (5 min) fail                                                                                           |

The tests in `tests/fetch.rs` cover these, against HTTP servers on 127.0.0.1.

What the policy cannot prevent: an allowed origin that echoes request headers
back (an echo service, a debug endpoint) hands the key to the program. Allow
the origins a key is for, not more.

## Not there yet

- **Streaming.** A response arrives whole. Streaming a model's answer to a
  UI as it comes needs another design: a turn per chunk is too many commits.
- **Cancelling.** `signal` is accepted and ignored, but for one already
  aborted: a request the host has sent cannot be taken back.
- **One policy per deployment.** Programs of different trust might need
  their own policies: the binding does not know which program calls it.
- **Limits per run.** No cap yet on how many requests a run has in flight.
- **Libraries.** `fetch`, `Headers` and `Response` are there, but QuickJS lacks
  `URL`, `TextEncoder`, `setTimeout` and `AbortController`, which SDKs such as
  Anthropic's use. Running one unchanged is the next experiment.
