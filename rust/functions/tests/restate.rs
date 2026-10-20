//! The services against a real Restate server: `make test-functions`, which
//! downloads one to RESTATE_SERVER. Ignored by a plain
//! `cargo test`, which has no server.
mod fakes;

use functions::{Binding, Bindings, GuestError, HostCall, Programs, Runtime};
use futures::future::BoxFuture;
use restate_sdk::prelude::*;
use serde_json::{Value, json};
use std::{
    net::TcpListener,
    path::PathBuf,
    process::{Child, Command},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};

const INGRESS: &str = "http://127.0.0.1:8080";
const ADMIN: &str = "http://127.0.0.1:9070";

/// The service a run calls. `slow` takes half a second, and counts its runs.
struct Doubler;

static SLOW_RUNS: AtomicU32 = AtomicU32::new(0);

#[restate_sdk::service]
impl Doubler {
    #[handler]
    async fn double(&self, _ctx: Context<'_>, n: Json<i64>) -> HandlerResult<Json<i64>> {
        Ok(Json(n.into_inner() * 2))
    }

    #[handler]
    async fn slow(&self, ctx: Context<'_>, n: Json<i64>) -> HandlerResult<Json<i64>> {
        ctx.run(|| async {
            SLOW_RUNS.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .await?;
        ctx.sleep(Duration::from_millis(500)).await?;
        Ok(Json(n.into_inner() * 2))
    }
}

/// A binding that answers each call, after 200 ms, with its key and input,
/// and records the call.
struct Record;

/// Every call `Record` was made: its ID, key and input.
static RECORDED: Mutex<Vec<(String, String, Value)>> = Mutex::new(Vec::new());

impl Binding for Record {
    fn call(&self, call: HostCall) -> BoxFuture<'static, Result<Value, GuestError>> {
        let answer = json!({"key": call.idempotency_key, "input": call.input});
        RECORDED
            .lock()
            .unwrap()
            .push((call.id, call.idempotency_key, call.input));
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            Ok(answer)
        })
    }

    fn deduplicates(&self) -> bool {
        true
    }
}

/// `restate-server` in a directory of its own, killed when dropped.
struct Server {
    process: Child,
    _dir: tempfile::TempDir,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

async fn server() -> Server {
    let binary = std::env::var("RESTATE_SERVER")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../target/restate/1.8.0/restate-server")
        });
    let dir = tempfile::tempdir().unwrap();
    // Kept for a failed test, beside its binary.
    let log = std::fs::File::create(PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("restate.log"))
        .unwrap();
    let process = Command::new(&binary)
        .arg("--base-dir")
        .arg(dir.path())
        .env("RESTATE_BIND_IP", "127.0.0.1")
        .env("RESTATE_LISTEN_MODE", "tcp")
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .spawn()
        .unwrap_or_else(|e| panic!("{}: {e} (make test-functions gets it)", binary.display()));
    let server = Server { process, _dir: dir };
    let client = reqwest::Client::new();
    wait(|| async {
        client
            .get(format!("{ADMIN}/health"))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
    })
    .await;
    server
}

async fn wait<F, Fut>(mut done: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = Instant::now() + Duration::from_secs(60);
    while !done().await {
        assert!(Instant::now() < deadline, "Timed out");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The test programs: `counter` calls `Doubler`, `failing` throws, `attach`
/// waits on a slow call and a timer, `race` settles before its loser, `host`
/// calls bindings, `keyed` calls under a key of its own.
fn programs() -> Arc<Programs> {
    static PROGRAMS: OnceLock<Arc<Programs>> = OnceLock::new();
    PROGRAMS
        .get_or_init(|| {
            let runtime = Arc::new(Runtime::new().unwrap());
            let programs = Programs::new(runtime)
                .compile("counter", COUNTER)
                .and_then(|p| p.compile("failing", FAILING))
                .and_then(|p| p.compile("attach", ATTACH))
                .and_then(|p| p.compile("race", RACE))
                .and_then(|p| p.compile("host", HOST))
                .and_then(|p| p.compile("keyed", KEYED))
                .and_then(|p| p.compile("approvals", APPROVALS))
                .and_then(|p| p.compile_python("approvals_py", APPROVALS_PY))
                .unwrap();
            Arc::new(programs)
        })
        .clone()
}

/// Serve the programs and `Doubler`, and register them with the server.
async fn serve() -> u16 {
    serve_programs(&programs()).await
}

/// The `workspace` service's directories, for the whole test binary.
fn workspaces() -> &'static std::path::Path {
    static DIR: OnceLock<tempfile::TempDir> = OnceLock::new();
    DIR.get_or_init(|| tempfile::tempdir().unwrap()).path()
}

/// Serve `programs` with a scripted `claude.messages` and `record` as
/// bindings, and `Doubler` and `workspace`, and register them with the
/// server.
async fn serve_programs(programs: &Arc<Programs>) -> u16 {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing_subscriber::filter::LevelFilter::DEBUG)
        .with_test_writer()
        .try_init();
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let cache = functions::page_cache().await.unwrap();
    let bindings = Bindings::new()
        .with("claude.messages", fakes::Claude)
        .with("record", Record);
    let budget = functions::Budget::from_env().unwrap();
    let endpoint = functions::bind(Endpoint::builder(), programs, &cache, &budget, &bindings)
        .unwrap()
        .bind(Doubler)
        .bind(functions::Workspace::new(workspaces()))
        .build();
    tokio::spawn(
        HttpServer::new(endpoint).listen_and_serve(format!("127.0.0.1:{port}").parse().unwrap()),
    );
    let client = reqwest::Client::new();
    wait(|| async {
        client
            .post(format!("{ADMIN}/deployments"))
            .json(&json!({"uri": format!("http://127.0.0.1:{port}"), "force": true}))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
    })
    .await;
    port
}

async fn call(path: &str, body: Value) -> (u16, Value) {
    let response = match reqwest::Client::new()
        .post(format!("{INGRESS}/{path}"))
        .json(&body)
        .timeout(Duration::from_secs(30))
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => panic!("{path}: {error}\n{}", invocations().await),
    };
    let status = response.status().as_u16();
    let text = response.text().await.unwrap();
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

/// The server's invocations, for a failed call.
async fn invocations() -> String {
    let query = "SELECT target, status, last_failure, last_failure_error_code FROM sys_invocation";
    let response = reqwest::Client::new()
        .post(format!("{ADMIN}/query"))
        .header("accept", "application/json")
        .json(&json!({ "query": query }))
        .send()
        .await;
    match response {
        Ok(response) => response.text().await.unwrap_or_default(),
        Err(error) => error.to_string(),
    }
}

const COUNTER: &str = r#"export async function run(input, ctx) {
  const n = ((await ctx.kv.get('count')) ?? 0) + input;
  await ctx.kv.set('count', n);
  const doubled = await ctx.rpc.call({ service: 'Doubler', handler: 'double' }, n);
  const started = Date.now();
  await ctx.sleep(200);
  return { n, doubled, slept: Date.now() - started >= 200 };
}"#;

const FAILING: &str = r#"export async function run() {
  await ctx_free();
}
async function ctx_free() { throw new RangeError('no luck'); }"#;

const ATTACH: &str = r#"export async function run(n, ctx) {
  const doubled = ctx.rpc.call({ service: 'Doubler', handler: 'slow' }, n);
  await ctx.sleep(50);
  return await doubled;
}"#;

const RACE: &str = r#"export async function run(input, ctx) {
  if (input === 'late') return await ctx.kv.get('late');
  const slow = ctx.rpc.call({ service: 'Doubler', handler: 'slow' }, 21);
  slow.then((v) => ctx.kv.set('late', v));
  return await Promise.race([ctx.sleep(50).then(() => 'timer'), slow]);
}"#;

const HOST: &str = r#"export async function run(n, ctx) {
  const call = ctx.call('record', { n });
  await ctx.sleep(50);
  const errors = [];
  for (const [name, options] of [['missing'], ['claude.messages', { idempotencyKey: 'k' }]]) {
    try {
      await ctx.call(name, {}, options);
    } catch (error) {
      errors.push(error.message);
    }
  }
  const mine = await ctx.call('record', { n: n + 1 }, { idempotencyKey: `mine-${n}` });
  return { first: await call, mine, errors };
}"#;

const KEYED: &str = r#"export async function run(n, ctx) {
  const once = () => ctx.rpc.call({ service: 'Doubler', handler: 'slow' }, n, { idempotencyKey: `slow-${n}` });
  return [await once(), await once()];
}"#;

const APPROVALS: &str = r#"export async function refund(order, ctx) {
  const verdict = await ctx.signal('approval', { order });
  const note = await ctx.signal('note');
  return { verdict, note };
}
export async function race(_, ctx) {
  return await Promise.race([ctx.signal('never', 'raced'), ctx.sleep(50).then(() => 'timer')]);
}
export async function both(_, ctx) {
  return await Promise.all([ctx.signal('vote', 1), ctx.signal('vote', 2)]);
}"#;

const APPROVALS_PY: &str = r#"
async def refund(order, ctx):
    verdict = await ctx.signal("approval", {"order": order})
    return {"verdict": verdict}
"#;

/// Send `body` to `path` without waiting: the invocation's id.
async fn send(path: &str, body: Value) -> String {
    let (status, sent) = call(&format!("{path}/send"), body).await;
    assert_eq!(status, 202, "{sent}");
    sent["invocationId"].as_str().unwrap().to_owned()
}

/// The answer of the invocation `id`, once there is one.
async fn attach(id: &str) -> (u16, Value) {
    let response = reqwest::Client::new()
        .get(format!("{INGRESS}/restate/invocation/{id}/attach"))
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    (status, response.json().await.unwrap_or(Value::Null))
}

/// The waits of `object`, once `done` says they are as expected.
async fn waits(object: &str, done: impl Fn(&[Value]) -> bool) -> Vec<Value> {
    let started = Instant::now();
    loop {
        let (status, pending) = call(&format!("{object}/pending"), Value::Null).await;
        assert_eq!(status, 200, "{pending}");
        let pending = pending.as_array().unwrap().clone();
        if done(&pending) {
            return pending;
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "{object}: {pending:?}\n{}",
            invocations().await
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs restate-server: make test-functions"]
async fn a_run_waits_for_a_signal_from_outside() {
    let _server = server().await;
    serve().await;

    // A signal for a name no one waits for is dropped, unless kept.
    let dropped = json!({"name": "note", "value": "early"});
    assert_eq!(
        call("approvals/alice/signal", dropped).await,
        (200, json!({"resolved": null}))
    );
    let kept = json!({"name": "note", "value": "kept", "keep": true});
    assert_eq!(
        call("approvals/alice/signal", kept).await,
        (200, json!({"resolved": null}))
    );

    let refund = send("approvals/alice/refund", json!("103")).await;
    let [wait] = &waits("approvals/alice", |w| !w.is_empty()).await[..] else {
        unreachable!()
    };
    assert_eq!(
        (&wait["name"], &wait["input"]),
        (&json!("approval"), &json!({"order": "103"}))
    );
    assert!(wait["since"].as_u64().unwrap() > 0, "{wait}");
    let approve = json!({"id": wait["id"], "value": "approved"});
    assert_eq!(
        call("approvals/alice/signal", approve).await,
        (200, json!({"resolved": wait["id"]}))
    );
    // The second wait takes the kept signal at once.
    assert_eq!(
        attach(&refund).await,
        (200, json!({"verdict": "approved", "note": "kept"}))
    );
    waits("approvals/alice", |w| w.is_empty()).await;

    // A signal by name resolves the oldest wait for it.
    let both = send("approvals/bob/both", Value::Null).await;
    waits("approvals/bob", |w| w.len() == 2).await;
    for value in ["first", "second"] {
        let (status, resolved) = call(
            "approvals/bob/signal",
            json!({"name": "vote", "value": value}),
        )
        .await;
        assert_eq!(status, 200);
        assert!(resolved["resolved"].is_string(), "{resolved}");
    }
    assert_eq!(attach(&both).await, (200, json!(["first", "second"])));
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs restate-server: make test-functions"]
async fn a_python_run_waits_for_a_signal() {
    let _server = server().await;
    serve().await;

    let refund = send("approvals_py/erin/refund", json!("105")).await;
    let [wait] = &waits("approvals_py/erin", |w| !w.is_empty()).await[..] else {
        unreachable!()
    };
    assert_eq!(wait["input"], json!({"order": "105"}));
    let approve = json!({"name": "approval", "value": "approved"});
    assert_eq!(
        call("approvals_py/erin/signal", approve).await,
        (200, json!({"resolved": wait["id"]}))
    );
    assert_eq!(attach(&refund).await, (200, json!({"verdict": "approved"})));
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs restate-server: make test-functions"]
async fn a_wait_survives_a_crash_and_one_no_one_answers_is_forgotten() {
    let _server = server().await;
    serve().await;

    // The wait's turn commits, then its attempt crashes: the retry waits
    // again, under the same key, and joins the same wait.
    functions::faults::crash_after_commit(0, 1);
    let refund = send("approvals/carol/refund", json!("104")).await;
    let [wait] = &waits("approvals/carol", |w| !w.is_empty()).await[..] else {
        unreachable!()
    };
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        waits("approvals/carol", |_| true).await.len(),
        1,
        "one wait, after the retry"
    );
    assert_eq!(functions::faults::pending(), 0);
    call(
        "approvals/carol/signal",
        json!({"id": wait["id"], "value": "approved"}),
    )
    .await;
    call(
        "approvals/carol/signal",
        json!({"name": "note", "value": "thanks"}),
    )
    .await;
    assert_eq!(
        attach(&refund).await,
        (200, json!({"verdict": "approved", "note": "thanks"}))
    );

    // The timer wins the race: the signal's wait is forgotten.
    assert_eq!(
        call("approvals/dave/race", Value::Null).await,
        (200, json!("timer"))
    );
    waits("approvals/dave", |w| w.is_empty()).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs restate-server: make test-functions"]
async fn runs_wait_on_rpcs_and_timers_and_share_their_scope() {
    let _server = server().await;
    serve().await;

    let (status, first) = call("counter/alice/run", json!(20)).await;
    assert_eq!(
        (status, first),
        (200, json!({"n": 20, "doubled": 40, "slept": true}))
    );
    let (_, second) = call("counter/alice/run", json!(1)).await;
    assert_eq!(second, json!({"n": 21, "doubled": 42, "slept": true}));

    // Another scope has a KV of its own.
    let (_, bob) = call("counter/bob/run", json!(1)).await;
    assert_eq!(bob, json!({"n": 1, "doubled": 2, "slept": true}));

    // An application's error is the run's terminal error.
    let (status, error) = call("failing/alice/run", Value::Null).await;
    assert_eq!(status, 500);
    assert!(error.to_string().contains("RangeError: no luck"), "{error}");

    // There is no program to run but those served.
    let (status, _) = call("missing/alice/run", Value::Null).await;
    assert_eq!(status, 404);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs restate-server: make test-functions"]
async fn a_retried_attempt_attaches_to_its_calls() {
    let _server = server().await;
    serve().await;

    // The first turn commits and calls slow; the timer's turn commits, then
    // its attempt crashes while the call is still running. The retry restores
    // the timer's image and calls again, under the same idempotency key.
    let before = SLOW_RUNS.load(Ordering::SeqCst);
    functions::faults::crash_after_commit(1, 1);
    let (status, doubled) = call("attach/carol/run", json!(5)).await;
    assert_eq!((status, doubled), (200, json!(10)));
    assert_eq!(functions::faults::pending(), 0);
    assert_eq!(
        SLOW_RUNS.load(Ordering::SeqCst) - before,
        1,
        "slow ran once"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs restate-server: make test-functions"]
async fn bindings_are_called_with_their_input_and_key() {
    let _server = server().await;
    serve().await;
    RECORDED.lock().unwrap().clear();

    // The call is made after the first turn commits; the timer's turn
    // commits, then its attempt crashes while the call is still running. The
    // retry reads the input from the restored heap, and calls again under
    // the same key.
    functions::faults::crash_after_commit(1, 1);
    let (status, done) = call("host/erin/run", json!(5)).await;
    assert_eq!(status, 200, "{done}");
    assert_eq!(functions::faults::pending(), 0);
    let recorded = RECORDED.lock().unwrap().clone();
    let [
        (first, key, input),
        (again, key_again, input_again),
        (mine, mine_key, _),
    ] = &recorded[..]
    else {
        panic!("Expected three calls: {recorded:?}");
    };
    assert!(first.ends_with("/0"), "{first}");
    assert_eq!((again, key, key_again), (first, first, first));
    assert_eq!((input, input_again), (&json!({"n": 5}), &json!({"n": 5})));
    assert_ne!(mine, first);
    assert_eq!(mine_key, "mine-5");
    assert_eq!(
        done,
        json!({
            "first": {"key": first, "input": {"n": 5}},
            "mine": {"key": "mine-5", "input": {"n": 6}},
            "errors": [
                "No host binding \"missing\"",
                "claude.messages does not deduplicate: it takes no idempotency key",
            ],
        })
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs restate-server: make test-functions"]
async fn a_call_under_the_programs_key_is_made_once() {
    let _server = server().await;
    serve().await;

    let before = SLOW_RUNS.load(Ordering::SeqCst);
    let (status, doubled) = call("keyed/frank/run", json!(4)).await;
    assert_eq!((status, doubled), (200, json!([8, 8])));
    assert_eq!(
        SLOW_RUNS.load(Ordering::SeqCst) - before,
        1,
        "slow ran once"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs restate-server: make test-functions"]
async fn a_run_ends_when_main_settles() {
    let _server = server().await;
    serve().await;

    let (_, winner) = call("race/dave/run", json!("go")).await;
    assert_eq!(winner, json!("timer"));
    // The loser was dropped: its continuation never ran, even once it is done.
    tokio::time::sleep(Duration::from_millis(700)).await;
    let (_, late) = call("race/dave/run", json!("late")).await;
    assert_eq!(late, Value::Null);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs restate-server: make test-functions"]
async fn the_shipped_programs_do_what_they_say() {
    let _server = server().await;
    serve_programs(&functions::embedded().unwrap()).await;

    let (_, counted) = call("counter/alice/add", json!(20)).await;
    assert_eq!(counted, json!(20));
    assert_eq!(call("counter/alice/get", Value::Null).await.1, json!(20));
    let (_, memory) = call("memory/alice/run", json!(3)).await;
    assert_eq!(
        memory,
        json!({"tally": {"durable": 1, "javascript": 1, "is": 1}, "calls": 3})
    );
    let (_, fanned) = call("fanout/alice/run", json!(["bob", "carol", "alice"])).await;
    assert_eq!(fanned, json!({"bob": 1, "carol": 1, "alice": 21}));
    let (_, answered) = call("timeout/alice/run", json!(300)).await;
    assert_eq!(answered, json!("done after 300 ms"));
    let (_, timed_out) = call("timeout/alice/run", json!(3000)).await;
    assert_eq!(timed_out, json!("timed out"));
    let (_, primes) = call("primes/alice/count", json!(20000)).await;
    assert_eq!(primes, json!(2262));
    assert_eq!(
        call("primes/alice/progress", Value::Null).await.1,
        json!(19000)
    );
    call("notes/alice/add", json!("Buy milk")).await;
    call("notes/alice/add", json!("Call mum")).await;
    call("notes/bob/add", json!("Bob's own")).await;
    let (_, notes) = call("notes/alice/done", json!(1)).await;
    assert_eq!(
        notes,
        json!([{"id": 1, "text": "Buy milk", "done": 1}, {"id": 2, "text": "Call mum", "done": 0}])
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs restate-server: make test-functions"]
async fn the_shop_programs_compose() {
    let _server = server().await;
    serve_programs(&functions::embedded().unwrap()).await;
    let order = |customer: &str, items: Value| json!({"customer": customer, "items": items});
    call("inventory/widget/restock", json!(5)).await;
    call("inventory/gadget/restock", json!(1)).await;
    call("payments/alice/deposit", json!(100)).await;

    // Reserved, then charged.
    let both =
        json!([{"sku": "widget", "qty": 2, "price": 10}, {"sku": "gadget", "qty": 1, "price": 30}]);
    let (_, paid) = call("checkout/order-1/pay", order("alice", both.clone())).await;
    assert_eq!(paid, json!({"status": "paid", "total": 50}));
    // The gadget is gone: the widget reserved before it is released.
    let (_, cancelled) = call("checkout/order-2/pay", order("alice", both)).await;
    assert_eq!(cancelled["status"], "cancelled");
    assert_eq!(cancelled["reason"], "Error: Out of stock: 0 left, 1 wanted");
    // Declined: the reservation is released too.
    let one = json!([{"sku": "widget", "qty": 1, "price": 10}]);
    let (_, declined) = call("checkout/order-3/pay", order("bob", one)).await;
    assert_eq!(declined["reason"], "Error: Declined: balance 0, charge 10");
    // An order checks out once.
    let (_, again) = call("checkout/order-1/pay", order("alice", json!([]))).await;
    assert_eq!(again, paid);
    assert_eq!(
        call("inventory/widget/available", Value::Null).await.1,
        json!(3)
    );
    assert_eq!(
        call("inventory/gadget/available", Value::Null).await.1,
        json!(0)
    );
    assert_eq!(
        call("payments/alice/current", Value::Null).await.1,
        json!(50)
    );

    let (_, retried) = call("retry/a/run", json!({"failures": 2, "attempts": 5})).await;
    assert_eq!(retried["answer"], "ok after 3 calls");
    assert_eq!(retried["errors"].as_array().unwrap().len(), 2);
    let (status, gave_up) = call("retry/b/run", json!({"failures": 5, "attempts": 2})).await;
    assert_eq!(status, 500);
    assert_eq!(gave_up["message"], "Error: Gave up after 2 attempts");

    call("payments/carol/deposit", json!(25)).await;
    let plan = json!({"price": 10, "periods": 3, "every": 100});
    let (_, lapsed) = call("subscription/carol/start", plan).await;
    assert_eq!(
        lapsed,
        json!({
            "status": "lapsed",
            "charges": [{"period": 1, "balance": 15}, {"period": 2, "balance": 5}],
            "reason": "Error: Declined: balance 5, charge 10",
        })
    );

    // A refund waits for a manager's approval, then pays back.
    call("payments/dana/deposit", json!(100)).await;
    let refund = send(
        "refunds/dana/request",
        json!({"amount": 30, "reason": "broken"}),
    )
    .await;
    waits("refunds/dana", |w| w.len() == 1).await;
    let approve = json!({"name": "approval", "value": "approved"});
    call("refunds/dana/signal", approve).await;
    assert_eq!(
        attach(&refund).await,
        (200, json!({"refunded": true, "balance": 130}))
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs restate-server: make test-functions"]
async fn the_assistant_shops_with_tools() {
    let _server = server().await;
    serve_programs(&functions::embedded().unwrap()).await;
    fakes::REQUESTS.lock().unwrap().clear();
    call("inventory/widget/restock", json!(5)).await;
    call("payments/dana/deposit", json!(100)).await;

    let (_, answered) = call("assistant/dana/chat", json!("Two widgets, please")).await;
    assert_eq!(answered, json!({"answer": "Bought 2 widgets."}));
    assert_eq!(
        call("inventory/widget/available", Value::Null).await.1,
        json!(3)
    );
    assert_eq!(
        call("payments/dana/current", Value::Null).await.1,
        json!(80)
    );

    let requests = fakes::REQUESTS.lock().unwrap().clone();
    let tools: Vec<&str> = requests[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(tools, ["catalog", "balance", "buy"]);
    // Both tools' results, in one message.
    let results = requests[1]["messages"][2]["content"].as_array().unwrap();
    let catalog: Value = serde_json::from_str(results[0]["content"].as_str().unwrap()).unwrap();
    assert_eq!(
        catalog[0],
        json!({"sku": "widget", "price": 10, "stock": 5})
    );
    assert_eq!(results[1]["content"], "100");
    let bought = &requests[2]["messages"][4]["content"][0]["content"];
    assert_eq!(bought, r#"{"status":"paid","total":20}"#);

    // The next run continues the conversation.
    call("assistant/dana/chat", json!("Two more")).await;
    let requests = fakes::REQUESTS.lock().unwrap().clone();
    assert_eq!(requests[3]["messages"].as_array().unwrap().len(), 7);
    assert_eq!(
        call("payments/dana/current", Value::Null).await.1,
        json!(60)
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs restate-server: make test-functions"]
async fn the_coder_works_in_its_workspace() {
    let _server = server().await;
    serve_programs(&functions::embedded().unwrap()).await;
    fakes::REQUESTS.lock().unwrap().clear();

    let (status, done) = call("coder/hello/work", json!({"task": "Say goodbye"})).await;
    assert_eq!(
        (status, done),
        (200, json!({"answer": "Done.", "steps": 4}))
    );
    let script = std::fs::read_to_string(workspaces().join("hello/bin/hello.sh")).unwrap();
    assert_eq!(script, "echo goodbye from $1");

    // Each step's tool results, as the model got them in the next request.
    let requests = fakes::REQUESTS.lock().unwrap().clone();
    let results = |step: usize| -> Vec<(String, bool)> {
        let messages = requests[step]["messages"].as_array().unwrap();
        let results = messages.last().unwrap()["content"].as_array().unwrap();
        results
            .iter()
            .map(|r| {
                (
                    r["content"].as_str().unwrap().to_owned(),
                    r["is_error"] == true,
                )
            })
            .collect()
    };
    assert_eq!(
        results(1),
        [
            ("Wrote 18 bytes to bin/hello.sh".into(), false),
            ("exit 0\nhello from world\n".into(), false),
        ]
    );
    let failed = results(2);
    assert_eq!(failed[0], ("bin/hello.sh: text not found".into(), true));
    assert!(
        failed[1].0.starts_with("missing.txt: No such file"),
        "{failed:?}"
    );
    assert!(
        failed[2].0.contains("relative to the workspace"),
        "{failed:?}"
    );
    assert!(failed.iter().all(|(_, error)| *error));
    assert_eq!(
        results(3),
        [
            ("Edited bin/hello.sh".into(), false),
            ("exit 3\ngoodbye from world\noops\n".into(), false),
        ]
    );
    // The conversation's end is cached; the commands' environment is clean.
    let last = &requests[3]["messages"].as_array().unwrap().last().unwrap()["content"];
    assert_eq!(
        last.as_array().unwrap().last().unwrap()["cache_control"]["type"],
        "ephemeral"
    );
    assert!(requests[1]["messages"][2]["content"][0]["cache_control"].is_null());
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs restate-server: make test-functions"]
async fn a_workspace_command_sees_no_secrets_and_times_out() {
    let _server = server().await;
    serve_programs(&programs()).await;
    let bash = |command: &str, timeout: u64| {
        call(
            "workspace/env/bash",
            json!({"command": command, "timeout": timeout}),
        )
    };
    // SAFETY: set before any command runs, read only by them.
    unsafe { std::env::set_var("SECRET_FOR_TEST", "leaked") };
    let (_, env) = bash("echo \"[$SECRET_FOR_TEST]\" $HOME", 10).await;
    let home = workspaces().join("env");
    assert_eq!(env["output"], format!("[] {}\n", home.display()));
    let started = Instant::now();
    let (_, slow) = bash("sleep 30 & sleep 30", 1).await;
    assert_eq!(slow, json!({"exit": -1, "output": "Killed after 1 s"}));
    assert!(started.elapsed() < Duration::from_secs(10));
}
