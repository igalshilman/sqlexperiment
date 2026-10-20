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
    rootOutcome,
    // The program's module, once evaluated: its exported functions.
    program,
    programError;
  // Each request's promise, by ID, until delivered. The request goes to the
  // host at the next report, and is forgotten here, but for a call's input:
  // it stays, for the host to read when it makes the call (`input`).
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
  // A call's input as JSON, as JSON.stringify makes it, in one pass in C: a
  // long conversation costs one serialization, not a walk in JavaScript.
  // JSON.stringify escapes lone surrogates, which the host's JSON refuses.
  function toJson(value) {
    const json = JSON.stringify(value === undefined ? null : value);
    if (json === undefined) throw new TypeError('Expected JSON data, not a ' + typeof value);
    if (json.includes('\\ud') && /(?<!\\)(?:\\\\)*\\ud[89a-f][0-9a-f]{2}/.test(json))
      throw new TypeError('Host JSON strings must not contain lone UTF-16 surrogates');
    return json;
  }
  // A call's input: a primitive is checked now, an object as it is read.
  function callInput(input) {
    if (input === null || typeof input !== 'object') validate(input);
    return input;
  }
  // A call's options, added to its `operation`: an idempotency key of the
  // program's own, if any.
  function withOptions(operation, options) {
    if (options === undefined) return operation;
    if (!options || typeof options !== 'object' || Object.keys(options).some((key) => key !== 'idempotencyKey'))
      throw new TypeError('Expected call options { idempotencyKey? }');
    const key = options.idempotencyKey;
    if (key === undefined) return operation;
    if (typeof key !== 'string' || !key) throw new TypeError('An idempotency key is a nonempty string');
    validate(key);
    operation.idempotencyKey = key;
    return operation;
  }
  function kvKey(key) {
    if (typeof key !== 'string') throw new TypeError('KV key must be a string');
    validate(key);
    return key;
  }
  // A host call, in the turn: its answer, or the error it throws.
  function hostCall(operation, request) {
    if (runId === undefined) throw new Error('The database is unavailable while the module evaluates');
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
  // Issue `operation`; a call keeps its `input` here until it is answered.
  function request(operation, input) {
    if (runId === undefined) throw new Error('Durable operations are unavailable while the module evaluates');
    const id = runId + '/' + nextOperation++;
    issued.push({ id, ...operation });
    // A losing race may reject later, unobserved: no rejection tracker is
    // installed, so that is not an error.
    return new Promise((resolve, reject) =>
      pending.set(id, input === undefined ? { resolve, reject } : { resolve, reject, input }),
    );
  }
  // Bytes as base64, and text as UTF-8, which QuickJS lacks: for bodies.
  const BASE64 = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/';
  function toBase64(bytes) {
    let text = '';
    for (let i = 0; i < bytes.length; i += 3) {
      const n = (bytes[i] << 16) | ((bytes[i + 1] ?? 0) << 8) | (bytes[i + 2] ?? 0);
      text += BASE64[n >> 18] + BASE64[(n >> 12) & 63];
      text += i + 1 < bytes.length ? BASE64[(n >> 6) & 63] : '=';
      text += i + 2 < bytes.length ? BASE64[n & 63] : '=';
    }
    return text;
  }
  function fromBase64(text) {
    text = text.replace(/=+$/, '');
    const bytes = new Uint8Array((text.length * 3) >> 2);
    for (let i = 0, j = 0, n = 0, bits = 0; i < text.length; i++) {
      n = (n << 6) | BASE64.indexOf(text[i]);
      bits += 6;
      if (bits >= 8) bytes[j++] = (n >> (bits -= 8)) & 255;
    }
    return bytes;
  }
  function utf8(text) {
    const binary = unescape(encodeURIComponent(text));
    const bytes = new Uint8Array(binary.length);
    for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
    return bytes;
  }
  function fromUtf8(bytes) {
    let binary = '';
    for (let i = 0; i < bytes.length; i++) binary += String.fromCharCode(bytes[i]);
    try {
      return decodeURIComponent(escape(binary));
    } catch {
      // Not UTF-8: each byte as its own character, as Latin-1 would.
      return binary;
    }
  }
  function bodyBytes(body) {
    if (body instanceof Uint8Array) return body;
    if (body instanceof ArrayBuffer) return new Uint8Array(body);
    if (ArrayBuffer.isView(body)) return new Uint8Array(body.buffer, body.byteOffset, body.byteLength);
    return undefined;
  }

  // HTTP headers, as fetch's Headers: names are case-insensitive.
  class Headers {
    #fields = [];
    constructor(init) {
      if (init === undefined || init === null) return;
      const pairs =
        init instanceof Headers || Array.isArray(init) || typeof init[Symbol.iterator] === 'function'
          ? init
          : Object.entries(init);
      for (const pair of pairs) {
        if (pair.length !== 2) throw new TypeError('A header is a [name, value] pair');
        this.append(pair[0], pair[1]);
      }
    }
    append(name, value) {
      name = String(name).toLowerCase();
      if (!/^[!#$%&'*+\-.^_`|~0-9a-z]+$/.test(name)) throw new TypeError(`Invalid header name ${name}`);
      value = String(value).trim();
      if (/[\0\r\n]/.test(value)) throw new TypeError(`Invalid value for header ${name}`);
      this.#fields.push([name, value]);
    }
    set(name, value) {
      this.delete(name);
      this.append(name, value);
    }
    delete(name) {
      name = String(name).toLowerCase();
      this.#fields = this.#fields.filter(([field]) => field !== name);
    }
    get(name) {
      name = String(name).toLowerCase();
      const values = this.#fields.filter(([field]) => field === name).map(([, value]) => value);
      return values.length ? values.join(', ') : null;
    }
    getSetCookie() {
      return this.#fields.filter(([field]) => field === 'set-cookie').map(([, value]) => value);
    }
    has(name) {
      return this.get(name) !== null;
    }
    *entries() {
      const names = [...new Set(this.#fields.map(([name]) => name))].sort();
      for (const name of names) {
        if (name === 'set-cookie') for (const value of this.getSetCookie()) yield [name, value];
        else yield [name, this.get(name)];
      }
    }
    *keys() {
      for (const [name] of this.entries()) yield name;
    }
    *values() {
      for (const [, value] of this.entries()) yield value;
    }
    forEach(fn, self) {
      for (const [name, value] of this.entries()) fn.call(self, value, name, this);
    }
    [Symbol.iterator]() {
      return this.entries();
    }
  }

  // A response, as fetch's Response: its body is text, or bytes.
  const responseInit = Symbol('response');
  class Response {
    #body;
    #used = false;
    constructor(body = null, init = {}) {
      const { status = 200, statusText = '', headers } = init;
      if (!Number.isInteger(status) || status < 200 || status > 599)
        throw new RangeError('A response status is from 200 to 599');
      if (body !== null && body !== undefined && typeof body !== 'string' && !bodyBytes(body))
        throw new TypeError('A response body is a string or bytes');
      this.#body = body ?? null;
      this.status = status;
      this.statusText = String(statusText);
      this.headers = new Headers(headers);
      this.url = init[responseInit]?.url ?? '';
      this.redirected = init[responseInit]?.redirected ?? false;
      this.type = 'default';
    }
    static json(data, init = {}) {
      const headers = new Headers(init.headers);
      if (!headers.has('content-type')) headers.set('content-type', 'application/json');
      return new Response(JSON.stringify(data), { ...init, headers });
    }
    get ok() {
      return this.status >= 200 && this.status <= 299;
    }
    get bodyUsed() {
      return this.#used;
    }
    #take() {
      if (this.#used) throw new TypeError('The body has already been read');
      this.#used = true;
      return this.#body;
    }
    async text() {
      const body = this.#take();
      return body === null ? '' : typeof body === 'string' ? body : fromUtf8(bodyBytes(body));
    }
    async json() {
      return JSON.parse(await this.text());
    }
    async bytes() {
      const body = this.#take();
      return body === null ? new Uint8Array(0) : typeof body === 'string' ? utf8(body) : bodyBytes(body).slice();
    }
    async arrayBuffer() {
      return (await this.bytes()).buffer;
    }
    clone() {
      if (this.#used) throw new TypeError('The body has already been read');
      return received(this.#body, this);
    }
  }
  // A response with any status (a 3xx, after a manual redirect), as `from`
  // describes it.
  function received(body, from) {
    const response = new Response(body, {
      headers: from.headers,
      [responseInit]: { url: from.url, redirected: from.redirected },
    });
    response.status = from.status;
    response.statusText = from.statusText;
    return response;
  }

  // An HTTP request, sent by the host under its policy (the binding `fetch`):
  // the request stays in the heap until it is answered, and is sent again
  // after a crash. No cookies are kept: a program sends its own.
  function fetch(input, init = {}) {
    try {
      const url = typeof input === 'string' ? input : (input?.url ?? input?.href ?? String(input));
      const from = typeof input === 'object' && input !== null && 'url' in input ? input : {};
      let method = String(init.method ?? from.method ?? 'GET');
      if (/^(get|head|post|put|delete|options|patch)$/i.test(method)) method = method.toUpperCase();
      const headers = new Headers(init.headers ?? from.headers);
      const body = init.body ?? null;
      const sent = { url: String(url), method, headers: [] };
      if (body !== null) {
        if (method === 'GET' || method === 'HEAD') throw new TypeError(`A ${method} request has no body`);
        if (typeof body === 'string') {
          sent.body = body;
          if (!headers.has('content-type')) headers.set('content-type', 'text/plain;charset=UTF-8');
        } else if (bodyBytes(body)) sent.bodyBase64 = toBase64(bodyBytes(body));
        else if (body instanceof JsonBody) {
          sent.json = body.value;
          if (!headers.has('content-type')) headers.set('content-type', 'application/json');
        } else throw new TypeError('A request body is a string or bytes: JSON.stringify an object');
      }
      for (const pair of headers) sent.headers.push(pair);
      const redirect = init.redirect ?? 'follow';
      if (!['follow', 'manual', 'error'].includes(redirect))
        throw new TypeError('redirect is "follow", "manual" or "error"');
      sent.redirect = redirect;
      if (init.signal?.aborted) throw init.signal.reason ?? new Error('The request was aborted');
      return request({ kind: 'call', name: 'fetch' }, sent).then((answer) => {
        const body = 'bodyBase64' in answer ? fromBase64(answer.bodyBase64) : answer.body;
        return received(body, answer);
      });
    } catch (error) {
      return Promise.reject(error);
    }
  }
  // A JSON body the host serializes as it sends it: `fetch.json(value)`. The
  // program keeps `value` as it is, and no copy of it as text.
  class JsonBody {
    constructor(value) {
      this.value = value;
    }
  }
  fetch.json = (value) => new JsonBody(callInput(value));
  globalThis.fetch = fetch;
  globalThis.Headers = Headers;
  globalThis.Response = Response;

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
      // The input is read when the call is made, and again after a crash:
      // do not change it until the call is answered.
      call(target, input = null, options) {
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
        return request(withOptions({ kind: 'rpc', target }, options), callInput(input));
      },
    }),
    // The host binding `name`, such as `fetch`; its input is read
    // as an RPC's is.
    call(name, input = null, options) {
      if (typeof name !== 'string' || !name) throw new TypeError('A binding name is a nonempty string');
      validate(name);
      return request(withOptions({ kind: 'call', name }, options), callInput(input));
    },
    // Wait for the signal `name`, which the object's `signal` handler gives
    // (by its wait's id, or to the oldest wait for its name), and resolve
    // with its value. `input` says what it waits for, as `pending` lists it.
    signal(name, input = null) {
      if (typeof name !== 'string' || !name) throw new TypeError('A signal name is a nonempty string');
      validate(name);
      return request({ kind: 'signal', name, input: copy(input) });
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
    // The input of the pending call `id`, as an outcome: its JSON, or why it
    // has none.
    input(id) {
      try {
        const entry = pending.get(id);
        if (!entry || !('input' in entry)) throw new Error('No pending call ' + id);
        // An object is checked here, as JSON.stringify sees it: in one pass.
        return '{"ok":true,"value":' + toJson(entry.input) + '}';
      } catch (error) {
        return JSON.stringify(failure(error));
      }
    },
    // The program's module, as it evaluates: its exports are the handlers.
    bind(namespace) {
      namespace.then(
        (exports) => (program = exports),
        (error) => (programError = error),
      );
    },
    handlers() {
      if (programError) throw programError;
      if (!program) throw new Error('The program did not finish evaluating');
      return JSON.stringify(Object.keys(program).filter((name) => typeof program[name] === 'function'));
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
      const { handler, input } = JSON.parse(json);
      const exported = program[handler];
      // The handler runs in the microtask pump, including synchronous exceptions.
      Promise.resolve()
        .then(() => {
          if (typeof exported !== 'function') throw new TypeError(`No exported function ${handler}`);
          return exported(input, durable);
        })
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
      // The run ends when its handler settles: requests still pending are
      // dropped, and their continuations never run.
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
