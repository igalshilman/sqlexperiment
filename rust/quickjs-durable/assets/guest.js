(() => {
  let runId,
    scope,
    nextOperation = 0,
    now = 0,
    maxTime = 0,
    random = 1,
    rootOutcome;
  // Each request's promise, by ID, until delivered. The request itself goes
  // to the host at the next report, and is forgotten here; but a model
  // request stays, and goes only when the host asks for it (`request`).
  const pending = new Map();
  // Since the last report: each request's fields, and its value as JSON.
  let issued = [];
  // Rust JSON strings are Unicode scalar values. Arbitrary UTF-16 remains valid
  // inside the VM, but cannot be sent through this JSON-only host interface.
  const errorText = (value) => String(value).replace(/[\uD800-\uDFFF]/gu, '\uFFFD');
  const failure = (error) => ({
    ok: false,
    error: {
      name: errorText((error && error.name) || 'Error'),
      message: errorText((error && error.message) || error),
    },
  });
  function checkString(text) {
    if (/[\uD800-\uDFFF]/u.test(text)) throw new TypeError('Host JSON strings must not contain lone UTF-16 surrogates');
  }
  // A value as JSON, as JSON.stringify makes it, in one pass in C: a long
  // conversation costs one serialization, not a walk in JavaScript. undefined
  // is null; a function or a symbol is no value at all. JSON.stringify escapes
  // lone surrogates, and only those.
  function toJson(value) {
    const json = JSON.stringify(value);
    if (json === undefined) {
      if (value === undefined) return 'null';
      throw new TypeError('Expected JSON data, not a ' + typeof value);
    }
    if (json.includes('\\ud') && /(?<!\\)(?:\\\\)*\\ud[89a-f][0-9a-f]{2}/.test(json))
      throw new TypeError('Host JSON strings must not contain lone UTF-16 surrogates');
    return json;
  }
  function kvKey(key) {
    if (typeof key !== 'string') throw new TypeError('KV key must be a string');
    checkString(key);
    return key;
  }
  // Issue a request: its fields, and `value` as JSON in its field `name`.
  // `kept` stays here until delivered, for the host to ask for.
  function issue(fields, name, json, kept) {
    if (runId === undefined) throw new Error('Durable operations are unavailable during application initialization');
    const id = runId + '/' + nextOperation++;
    issued.push({ fields: { id, ...fields }, name, json });
    // A losing race may reject later, unobserved: no rejection tracker is
    // installed, so that is not an error.
    return new Promise((resolve, reject) => pending.set(id, { resolve, reject, kept }));
  }
  globalThis.durable = Object.freeze({
    // The scope: the key of the object this program is served as.
    get key() {
      return scope;
    },
    kv: Object.freeze({
      get(key) {
        return issue({ kind: 'kv.get', key: kvKey(key) });
      },
      set(key, value) {
        return issue({ kind: 'kv.set', key: kvKey(key) }, 'value', toJson(value));
      },
      delete(key) {
        return issue({ kind: 'kv.delete', key: kvKey(key) });
      },
    }),
    rpc: Object.freeze({
      call(target, input = null) {
        target = JSON.parse(toJson(target));
        if (
          !target ||
          Array.isArray(target) ||
          typeof target !== 'object' ||
          typeof target.service !== 'string' ||
          !target.service ||
          typeof target.handler !== 'string' ||
          !target.handler ||
          (target.key !== undefined && typeof target.key !== 'string') ||
          Object.keys(target).some((key) => !['service', 'handler', 'key'].includes(key))
        ) {
          throw new TypeError('Expected RPC target { service, handler, key? }');
        }
        return issue({ kind: 'rpc', target }, 'input', toJson(input));
      },
    }),
    // Claude's Messages API: `request` is its JSON body, the result its
    // response. The host sends it, reading it from here; it is not stored,
    // so do not change it while it waits.
    claude: Object.freeze({
      messages(request) {
        if (request === null || typeof request !== 'object' || Array.isArray(request))
          throw new TypeError('Expected a Messages API request object');
        return issue({ kind: 'model' }, undefined, undefined, request);
      },
    }),
    call(name, args = null) {
      if (typeof name !== 'string' || !name) throw new TypeError('Operation name must be nonempty');
      checkString(name);
      return issue({ kind: 'call', name }, 'args', toJson(args));
    },
    sleep(ms) {
      if (!Number.isSafeInteger(ms) || ms < 0 || !Number.isSafeInteger(now + ms) || now + ms > maxTime)
        throw new RangeError('Invalid sleep duration');
      return issue({ kind: 'sleep', deadline: now + ms });
    },
  });
  Math.random = () => {
    random ^= random << 13;
    random ^= random >>> 17;
    random ^= random << 5;
    return (random >>> 0) / 4294967296;
  };
  globalThis.__durable = {
    // Runtime-only scheduling hook. Application code uses the frozen ctx API.
    pause() {
      issue({ kind: 'resume' });
    },
    // The kept request of `id`, as an outcome: its JSON, or why it has none.
    request(id) {
      const entry = pending.get(id);
      try {
        if (!entry || entry.kept === undefined) throw new Error('No kept request ' + id);
        return '{"ok":true,"value":' + toJson(entry.kept) + '}';
      } catch (error) {
        return JSON.stringify(failure(error));
      }
    },
    init(json) {
      if (runId !== undefined) throw new Error('Invocation is already initialized');
      const config = JSON.parse(json);
      runId = config.runId;
      scope = config.scope;
      now = config.now;
      maxTime = config.maxTime;
      random = config.seed || 1;
    },
    start(json) {
      const input = JSON.parse(json);
      // main runs in the microtask pump, including synchronous exceptions.
      Promise.resolve()
        .then(() => main(input, durable))
        .then(
          (value) => {
            try {
              rootOutcome = { ok: true, json: toJson(value) };
            } catch (error) {
              rootOutcome = failure(error);
            }
          },
          (error) => {
            rootOutcome = failure(error);
          },
        );
    },
    deliver(json) {
      const event = JSON.parse(json);
      now = Math.max(now, event.now);
      const entry = pending.get(event.operationId);
      if (!entry) throw new Error('Completion has no pending promise');
      pending.delete(event.operationId);
      if (event.outcome.ok) entry.resolve(event.outcome.value);
      else {
        const error = new Error(event.outcome.error.message);
        error.name = event.outcome.error.name;
        entry.reject(error);
      }
    },
    inspect() {
      if (!rootOutcome && pending.size === 0) rootOutcome = failure(new Error('Execution stalled: no durable wakeup'));
      // The run ends when main settles: requests still pending are dropped,
      // and their continuations never run.
      if (rootOutcome) {
        pending.clear();
        issued = [];
      }
      const status = !rootOutcome ? 'waiting' : rootOutcome.ok ? 'completed' : 'failed';
      // Built around the values' JSON, which is not serialized again.
      const requests = issued.map(({ fields, name, json }) => {
        const text = JSON.stringify(fields);
        return name === undefined ? text : text.slice(0, -1) + ',"' + name + '":' + json + '}';
      });
      let outcome = '';
      if (rootOutcome && rootOutcome.ok) outcome = ',"outcome":{"ok":true,"value":' + rootOutcome.json + '}';
      else if (rootOutcome) outcome = ',"outcome":' + JSON.stringify(rootOutcome);
      issued = [];
      const state = JSON.stringify({ nextOperation, now, status }).slice(1, -1);
      return '{"issued":[' + requests.join(',') + '],' + state + outcome + '}';
    },
  };
})();
