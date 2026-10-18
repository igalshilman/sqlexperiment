//! The services against a real Restate server (RESTATE_SERVER, or the one
//! `make restate-server` gets): `make test-qjs-restate`. Ignored by a plain
//! `cargo test`, which has no server.
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
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/restate/restate-server")
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
        .unwrap_or_else(|e| panic!("{}: {e} (make restate-server)", binary.display()));
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
/// waits on a slow call and a timer, `race` answers before its loser.
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
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing_subscriber::filter::LevelFilter::DEBUG)
        .with_test_writer()
        .try_init();
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let endpoint = quickjs_restate::bind(Endpoint::builder(), &programs())
        .bind(Doubler)
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

const COUNTER: &str = r#"async function main(input, ctx) {
  const n = ((await ctx.kv.get('count')) ?? 0) + input;
  await ctx.kv.set('count', n);
  const doubled = await ctx.rpc.call({ service: 'Doubler', handler: 'double' }, n);
  const started = Date.now();
  await ctx.sleep(200);
  return { n, doubled, slept: Date.now() - started >= 200 };
}"#;

const FAILING: &str = r#"async function main() {
  await ctx_free();
}
async function ctx_free() { throw new RangeError('no luck'); }"#;

const ATTACH: &str = r#"async function main(n, ctx) {
  const doubled = ctx.rpc.call({ service: 'Doubler', handler: 'slow' }, n);
  await ctx.sleep(50);
  return await doubled;
}"#;

const RACE: &str = r#"async function main(input, ctx) {
  if (input === 'late') return await ctx.kv.get('late');
  const slow = ctx.rpc.call({ service: 'Doubler', handler: 'slow' }, 21);
  slow.then((v) => ctx.kv.set('late', v));
  return await Promise.race([ctx.sleep(50).then(() => 'timer'), slow]);
}"#;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs restate-server: make test-qjs-restate"]
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
#[ignore = "needs restate-server: make test-qjs-restate"]
async fn a_retried_attempt_attaches_to_its_calls() {
    let _server = server().await;
    serve().await;

    // The first turn commits and calls slow; the timer's turn commits, then
    // its attempt crashes while the call is still running. The retry restores
    // the timer's image and calls again, under the same idempotency key.
    let before = SLOW_RUNS.load(Ordering::SeqCst);
    quickjs_restate::faults::crash_after_commit(1, 1);
    let (status, doubled) = call("attach/carol/run", json!(5)).await;
    assert_eq!((status, doubled), (200, json!(10)));
    assert_eq!(quickjs_restate::faults::pending(), 0);
    assert_eq!(
        SLOW_RUNS.load(Ordering::SeqCst) - before,
        1,
        "slow ran once"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs restate-server: make test-qjs-restate"]
async fn a_race_answers_first_and_drains_its_loser() {
    let _server = server().await;
    serve().await;

    let started = Instant::now();
    let (_, winner) = call("race/dave/run", json!("go")).await;
    assert_eq!(winner, json!("timer"));
    assert!(
        started.elapsed() < Duration::from_millis(450),
        "{:?}",
        started.elapsed()
    );
    // The next run of the scope waits for the drain: the loser's continuation ran.
    let (_, late) = call("race/dave/run", json!("late")).await;
    assert_eq!(late, json!(42));
}
