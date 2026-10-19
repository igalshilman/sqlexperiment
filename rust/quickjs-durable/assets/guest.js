(() => {
  // The host's one function, which the host defines before this bridge runs:
  // host(operation, request) answers a JSON string, synchronously.
  const host = globalThis.__host;
  delete globalThis.__host;
  let runId,
    scope,
    nextOperation = 0,
    now = 0,
    maxTime = 0,
    random = 1,
    rootOutcome;
  // Each request's promise, by ID, until delivered. The request itself goes
  // to the host at the next report, and is forgotten here.
  const pending = new Map();
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
  function validate(value, seen = new Set()) {
    if (typeof value === 'string') {
      if (/[\uD800-\uDFFF]/u.test(value))
        throw new TypeError('Host JSON strings must not contain lone UTF-16 surrogates');
      return;
    }
    if (value === null || typeof value === 'boolean') return;
    if (typeof value === 'number' && Number.isFinite(value)) return;
    if (typeof value !== 'object' || seen.has(value)) throw new TypeError('Expected acyclic JSON data');
    if (!Array.isArray(value) && Object.getPrototypeOf(value) !== Object.prototype)
      throw new TypeError('Expected plain JSON object');
    seen.add(value);
    for (const key of Object.keys(value)) validate(key);
    for (const item of Array.isArray(value) ? value : Object.values(value)) validate(item, seen);
    seen.delete(value);
  }
  function copy(value) {
    validate(value);
    // A primitive is already plain data, and the round trip below costs about
    // 40,000 Wasm instructions: only objects and arrays need it.
    if (value === null || typeof value !== 'object') return value;
    const result = JSON.parse(JSON.stringify(value));
    // Accessors can return a different value during JSON serialization. Validate
    // the actual plain data that will cross the boundary, not only the input.
    validate(result);
    return result;
  }
  function kvKey(key) {
    if (typeof key !== 'string') throw new TypeError('KV key must be a string');
    validate(key);
    return key;
  }
  // A host call, in the turn: its answer, or the error it throws.
  function hostCall(operation, request) {
    if (runId === undefined) throw new Error('The database is unavailable during application initialization');
    const answer = JSON.parse(host(operation, JSON.stringify(request)));
    if (answer.ok) return answer.value;
    const error = new Error(answer.error.message);
    error.name = answer.error.name;
    throw error;
  }
  // SQL values cross as JSON: a bigint as {$int}, bytes as {$blob} in hex.
  function sqlParam(value) {
    if (value === undefined || value === null) return null;
    if (typeof value === 'bigint') return { $int: String(value) };
    if (value instanceof Uint8Array) {
      let hex = '';
      for (const byte of value) hex += (byte < 16 ? '0' : '') + byte.toString(16);
      return { $blob: hex };
    }
    if (typeof value === 'number' || typeof value === 'string' || typeof value === 'boolean') {
      validate(value);
      return value;
    }
    throw new TypeError('An SQL parameter is a number, bigint, string, boolean, null or Uint8Array');
  }
  function sqlValue(value) {
    if (value === null || typeof value !== 'object') return value;
    if ('$int' in value) return BigInt(value.$int);
    const bytes = new Uint8Array(value.$blob.length / 2);
    for (let i = 0; i < bytes.length; i++) bytes[i] = parseInt(value.$blob.substr(i * 2, 2), 16);
    return bytes;
  }
  function query(sql, params) {
    if (typeof sql !== 'string') throw new TypeError('SQL must be a string');
    validate(sql);
    const rows = hostCall('db.sql', { sql, params: params.map(sqlParam) });
    for (const row of rows) for (const column in row) row[column] = sqlValue(row[column]);
    return rows;
  }
  function request(operation) {
    if (runId === undefined) throw new Error('Durable operations are unavailable during application initialization');
    const id = runId + '/' + nextOperation++;
    issued.push({ id, ...operation });
    // A losing race may reject later, unobserved: no rejection tracker is
    // installed, so that is not an error.
    return new Promise((resolve, reject) => pending.set(id, { resolve, reject }));
  }
  globalThis.durable = Object.freeze({
    // The scope: the key of the object this program is served as.
    get key() {
      return scope;
    },
    db: Object.freeze({
      // A tagged template: each ${value} is a parameter, never SQL text.
      sql(strings, ...values) {
        if (!Array.isArray(strings) || !Array.isArray(strings.raw))
          throw new TypeError('db.sql is a tagged template: db.sql`SELECT ...`');
        return query(strings.join('?'), values);
      },
      query(sql, params = []) {
        if (!Array.isArray(params)) throw new TypeError('Parameters are an array');
        return query(sql, params);
      },
      // fn's statements stand together, or not at all if it throws.
      transaction(fn) {
        if (typeof fn !== 'function') throw new TypeError('A transaction runs a function');
        hostCall('db.begin', null);
        let result;
        try {
          result = fn();
          if (result && typeof result.then === 'function')
            throw new TypeError('A transaction function is synchronous: it cannot await');
        } catch (error) {
          hostCall('db.rollback', null);
          throw error;
        }
        hostCall('db.commit', null);
        return result;
      },
    }),
    kv: Object.freeze({
      get(key) {
        return request({ kind: 'kv.get', key: kvKey(key) });
      },
      set(key, value) {
        return request({ kind: 'kv.set', key: kvKey(key), value: copy(value) });
      },
      delete(key) {
        return request({ kind: 'kv.delete', key: kvKey(key) });
      },
    }),
    rpc: Object.freeze({
      call(target, input = null) {
        target = copy(target);
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
        return request({ kind: 'rpc', target, input: copy(input) });
      },
    }),
    call(name, args = null) {
      if (typeof name !== 'string' || !name) throw new TypeError('Operation name must be nonempty');
      validate(name);
      return request({ kind: 'call', name, args: copy(args) });
    },
    sleep(ms) {
      if (!Number.isSafeInteger(ms) || ms < 0 || !Number.isSafeInteger(now + ms) || now + ms > maxTime)
        throw new RangeError('Invalid sleep duration');
      return request({ kind: 'sleep', deadline: now + ms });
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
      request({ kind: 'resume' });
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
              rootOutcome = { ok: true, value: copy(value === undefined ? null : value) };
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
      const report = JSON.stringify({
        issued,
        nextOperation,
        now,
        status,
        ...(rootOutcome === undefined ? {} : { outcome: rootOutcome }),
      });
      issued = [];
      return report;
    },
  };
})();
