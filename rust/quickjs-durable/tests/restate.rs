//! The services against a real Restate server: `make test-qjs`, which
//! downloads one to RESTATE_SERVER. Ignored by a plain
//! `cargo test`, which has no server.
mod fakes;

use quickjs_durable::{Programs, Runtime};
use restate_sdk::prelude::*;
use serde_json::{Value, json};
use std::{
    net::TcpListener,
    path::PathBuf,
    process::{Child, Command},
    sync::{
        Arc, OnceLock,
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
        // The stores read state through v8's unjournaled gets.
        .env("RESTATE_EXPERIMENTAL_ENABLE_PROTOCOL_V8", "true")
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .spawn()
        .unwrap_or_else(|e| panic!("{}: {e} (make test-qjs gets it)", binary.display()));
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
/// waits on a slow call and a timer, `race` settles before its loser.
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

/// Serve `programs`, `Doubler`, a scripted `claude` and `workspace`, and
/// register them with the server.
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
    let cache = quickjs_durable::page_cache().await.unwrap();
    let endpoint = quickjs_durable::bind(Endpoint::builder(), programs, &cache)
        .unwrap()
        .bind(Doubler)
        .bind(fakes::Claude)
        .bind(quickjs_durable::Workspace::new(workspaces()))
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

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs restate-server: make test-qjs"]
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
#[ignore = "needs restate-server: make test-qjs"]
async fn a_retried_attempt_attaches_to_its_calls() {
    let _server = server().await;
    serve().await;

    // The first turn commits and calls slow; the timer's turn commits, then
    // its attempt crashes while the call is still running. The retry restores
    // the timer's image and calls again, under the same idempotency key.
    let before = SLOW_RUNS.load(Ordering::SeqCst);
    quickjs_durable::faults::crash_after_commit(1, 1);
    let (status, doubled) = call("attach/carol/run", json!(5)).await;
    assert_eq!((status, doubled), (200, json!(10)));
    assert_eq!(quickjs_durable::faults::pending(), 0);
    assert_eq!(
        SLOW_RUNS.load(Ordering::SeqCst) - before,
        1,
        "slow ran once"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs restate-server: make test-qjs"]
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
#[ignore = "needs restate-server: make test-qjs"]
async fn the_shipped_programs_do_what_they_say() {
    let _server = server().await;
    serve_programs(&quickjs_durable::embedded().unwrap()).await;

    let (_, counted) = call("counter/alice/add", json!(20)).await;
    assert_eq!(counted, json!(20));
    assert_eq!(call("counter/alice/get", Value::Null).await.1, json!(20));
    let mut words = Vec::new();
    for _ in 0..3 {
        words.push(call("memory/alice/next", Value::Null).await.1);
    }
    assert_eq!(words, [json!("durable"), json!("javascript"), json!("is")]);
    let (_, tally) = call("memory/alice/tally", Value::Null).await;
    assert_eq!(
        tally,
        json!({"counts": {"durable": 1, "javascript": 1, "is": 1}, "calls": 3})
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
#[ignore = "needs restate-server: make test-qjs"]
async fn the_shop_programs_compose() {
    let _server = server().await;
    serve_programs(&quickjs_durable::embedded().unwrap()).await;
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
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs restate-server: make test-qjs"]
async fn the_assistant_shops_with_tools() {
    let _server = server().await;
    serve_programs(&quickjs_durable::embedded().unwrap()).await;
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
#[ignore = "needs restate-server: make test-qjs"]
async fn the_coder_works_in_its_workspace() {
    let _server = server().await;
    serve_programs(&quickjs_durable::embedded().unwrap()).await;
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
#[ignore = "needs restate-server: make test-qjs"]
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
