//! `fetch` from programs, against HTTP servers on this machine: what a request
//! carries, and what the policy keeps a program from reaching or reading.
mod common;

use common::{Store, attempt, catalog, make, programs, settle};
use functions::{Extensions, Fetch, FetchPolicy, Language, Programs};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

/// A request a server received.
#[derive(Clone, Debug)]
struct Received {
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

/// An HTTP server on 127.0.0.1, which records what it receives. Its paths:
/// `/echo` answers the request as JSON; `/bytes` answers bytes that are not
/// UTF-8; `/redirect/<status>?<url>` redirects; `/cookie` sets one;
/// `/big` answers 4 KiB; `/loop` redirects to itself; `/slow` answers in 2 s.
struct Server {
    origin: String,
    received: Arc<Mutex<Vec<Received>>>,
}

impl Server {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let received = Arc::new(Mutex::new(Vec::new()));
        let log = received.clone();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                tokio::spawn(serve(stream, log.clone()));
            }
        });
        Self { origin, received }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.origin)
    }

    fn received(&self) -> Vec<Received> {
        self.received.lock().unwrap().clone()
    }
}

async fn serve(mut stream: TcpStream, log: Arc<Mutex<Vec<Received>>>) {
    let mut data = Vec::new();
    let mut buffer = [0; 8192];
    let head_end = loop {
        let n = stream.read(&mut buffer).await.unwrap();
        if n == 0 {
            return;
        }
        data.extend_from_slice(&buffer[..n]);
        if let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            break end;
        }
    };
    let head = String::from_utf8(data[..head_end].to_vec()).unwrap();
    let mut lines = head.split("\r\n");
    let mut start = lines.next().unwrap().split(' ');
    let (method, path) = (start.next().unwrap(), start.next().unwrap());
    let headers: BTreeMap<String, String> = lines
        .map(|line| {
            let (name, value) = line.split_once(": ").unwrap();
            (name.to_lowercase(), value.to_owned())
        })
        .collect();
    let length = headers
        .get("content-length")
        .map_or(0, |n| n.parse().unwrap());
    let mut body = data[head_end + 4..].to_vec();
    while body.len() < length {
        let n = stream.read(&mut buffer).await.unwrap();
        body.extend_from_slice(&buffer[..n]);
    }
    let request = Received {
        method: method.into(),
        path: path.into(),
        headers,
        body,
    };
    log.lock().unwrap().push(request.clone());

    let (status, extra, content): (u16, String, Vec<u8>) = match path {
        "/echo" => {
            let echo = json!({
                "method": request.method,
                "headers": request.headers,
                "body": String::from_utf8_lossy(&request.body),
            });
            (
                200,
                "content-type: application/json\r\n".into(),
                echo.to_string().into(),
            )
        }
        "/bytes" => (200, String::new(), vec![0, 255, 1, 254]),
        "/cookie" => (
            200,
            "set-cookie: session=s3cret; Path=/\r\nset-cookie: theme=dark\r\n".into(),
            b"ok".to_vec(),
        ),
        "/big" => (200, String::new(), vec![b'x'; 4096]),
        "/loop" => (302, "location: /loop\r\n".into(), Vec::new()),
        "/slow" => {
            tokio::time::sleep(Duration::from_secs(2)).await;
            (200, String::new(), b"late".to_vec())
        }
        path if path.starts_with("/redirect/") => {
            let (status, location) = path["/redirect/".len()..].split_once('?').unwrap();
            (
                status.parse().unwrap(),
                format!("location: {location}\r\n"),
                Vec::new(),
            )
        }
        _ => (404, String::new(), Vec::new()),
    };
    let response = format!(
        "HTTP/1.1 {status} X\r\n{extra}content-length: {}\r\nconnection: close\r\n\r\n",
        content.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.write_all(&content).await;
}

/// Run `source`'s `main(input)` in `store`, sending its requests with
/// `fetch`, until it settles: its outcome.
async fn run(store: &Store, source: &str, input: Value, fetch: &Fetch) -> Value {
    drive(store, &programs(source), input, fetch).await
}

async fn drive(store: &Store, catalog: &Arc<Programs>, input: Value, fetch: &Fetch) -> Value {
    common::drive(store, catalog, input, &offered(fetch)).await
}

fn offered(fetch: &Fetch) -> Extensions {
    Extensions::new().with("fetch", fetch.clone())
}

/// The program: `main({url, init})` fetches, and answers what it received.
const FETCH: &str = r#"
export async function main({ url, init }) {
  try {
    const response = await fetch(url, init);
    const answer = {
      status: response.status,
      ok: response.ok,
      redirected: response.redirected,
      url: response.url,
      contentType: response.headers.get('content-type'),
      cookies: response.headers.getSetCookie(),
    };
    if (url.endsWith('/bytes')) answer.bytes = [...(await response.bytes())];
    else answer.text = await response.text();
    return answer;
  } catch (error) {
    return { error: `${error.name}: ${error.message}` };
  }
}
"#;

async fn fetch(policy: FetchPolicy, url: &str, init: Value) -> Value {
    run(
        &Store::default(),
        FETCH,
        json!({"url": url, "init": init}),
        &Fetch::new(policy),
    )
    .await
}

fn local(server: &Server) -> FetchPolicy {
    FetchPolicy::new()
        .allow(&server.origin)
        .allow_private_networks()
}

#[tokio::test]
async fn a_request_carries_its_method_headers_and_body() {
    let server = Server::start().await;
    let init = json!({
        "method": "post",
        "headers": {"content-type": "application/json", "x-trace": "t1"},
        "body": "{\"q\":\"héllo\"}",
    });
    let answer = fetch(local(&server), &server.url("/echo"), init).await;
    assert_eq!(answer["status"], 200, "{answer}");
    assert_eq!(answer["contentType"], "application/json");
    let echo: Value = serde_json::from_str(answer["text"].as_str().unwrap()).unwrap();
    assert_eq!(echo["method"], "POST");
    assert_eq!(echo["body"], "{\"q\":\"héllo\"}");
    assert_eq!(echo["headers"]["x-trace"], "t1");
    // Nothing the program did not send: no cookies, no referer, no credentials.
    assert_eq!(server.received()[0].path, "/echo");
    let headers = &server.received()[0].headers;
    for name in ["cookie", "referer", "authorization"] {
        assert!(!headers.contains_key(name), "{headers:?}");
    }

    let answer = fetch(local(&server), &server.url("/bytes"), json!({})).await;
    assert_eq!(answer["bytes"], json!([0, 255, 1, 254]));

    // Bytes go as they are.
    let init = json!({"method": "PUT"});
    let source = r#"
export async function main({ url }) {
  const response = await fetch(url, { method: 'PUT', body: new Uint8Array([0, 200, 7]) });
  return response.json();
}
"#;
    let fetch = Fetch::new(local(&server));
    let echo = run(
        &Store::default(),
        source,
        json!({"url": server.url("/echo"), "init": init}),
        &fetch,
    )
    .await;
    assert_eq!(echo["method"], "PUT");
    assert_eq!(server.received().last().unwrap().body, [0, 200, 7]);
}

#[tokio::test]
async fn only_allowed_origins_are_reached() {
    let allowed = Server::start().await;
    let other = Server::start().await;
    let answer = fetch(local(&allowed), &other.url("/echo"), json!({})).await;
    assert_eq!(
        answer["error"],
        format!("TypeError: Requests to {} are not allowed", other.origin)
    );
    assert!(other.received().is_empty());

    // Other schemes, credentials in a URL and the host's own headers are refused.
    for (url, init, error) in [
        (
            "file:///etc/passwd".to_owned(),
            json!({}),
            "Cannot fetch a file URL",
        ),
        (
            allowed.url("/echo").replace("http://", "http://user:pw@"),
            json!({}),
            "A URL with credentials cannot be fetched",
        ),
        (
            allowed.url("/echo"),
            json!({"headers": {"host": "x"}}),
            "Header host is set by the host",
        ),
        (
            allowed.url("/echo"),
            json!({"method": "TRACE"}),
            "Method TRACE is not allowed",
        ),
    ] {
        let answer = fetch(local(&allowed), &url, init).await;
        assert_eq!(answer["error"], format!("TypeError: {error}"), "{url}");
    }
    assert!(allowed.received().is_empty());
}

#[tokio::test]
async fn private_networks_are_not_reached_unless_allowed() {
    let server = Server::start().await;
    let port = server.origin.rsplit(':').next().unwrap();

    // An allowed origin on loopback, by its address and by a name.
    let answer = fetch(
        FetchPolicy::new().allow(&server.origin),
        &server.url("/echo"),
        json!({}),
    )
    .await;
    assert_eq!(
        answer["error"],
        format!("TypeError: Requests to {} are not allowed", server.origin)
    );
    let by_name = format!("http://localhost:{port}");
    let answer = fetch(
        FetchPolicy::new().allow(&by_name),
        &format!("{by_name}/echo"),
        json!({}),
    )
    .await;
    let error = answer["error"].as_str().unwrap();
    assert!(error.contains("localhost has no public address"), "{error}");

    // Any public origin is not the cloud's metadata, nor this machine.
    for url in [
        "http://169.254.169.254/latest/meta-data/".to_owned(),
        "http://[::ffff:127.0.0.1]/".to_owned(),
        format!("{by_name}/echo"),
    ] {
        let answer = fetch(FetchPolicy::new().allow_public(), &url, json!({})).await;
        assert!(
            answer["error"].as_str().unwrap().starts_with("TypeError: "),
            "{url}: {answer}"
        );
    }
    assert!(server.received().is_empty());
}

#[tokio::test]
async fn every_redirect_is_checked() {
    let server = Server::start().await;
    let other = Server::start().await;

    // To an origin the policy does not allow: refused, before it is reached.
    let to_other = server.url(&format!("/redirect/302?{}", other.url("/echo")));
    let answer = fetch(local(&server), &to_other, json!({})).await;
    assert_eq!(
        answer["error"],
        format!("TypeError: Requests to {} are not allowed", other.origin)
    );
    assert!(other.received().is_empty());

    // Within the origin: followed, and a POST's 303 becomes a GET.
    let init = json!({"method": "POST", "body": "x"});
    let answer = fetch(local(&server), &server.url("/redirect/303?/echo"), init).await;
    assert_eq!(answer["redirected"], true);
    assert_eq!(answer["url"], server.url("/echo"));
    let echo: Value = serde_json::from_str(answer["text"].as_str().unwrap()).unwrap();
    assert_eq!(
        (&echo["method"], &echo["body"]),
        (&json!("GET"), &json!(""))
    );

    // Not followed: as the program asks.
    let answer = fetch(
        local(&server),
        &server.url("/redirect/307?/echo"),
        json!({"redirect": "manual"}),
    )
    .await;
    assert_eq!(
        (&answer["status"], &answer["redirected"]),
        (&json!(307), &json!(false))
    );
    let answer = fetch(
        local(&server),
        &server.url("/redirect/307?/echo"),
        json!({"redirect": "error"}),
    )
    .await;
    assert!(
        answer["error"].as_str().unwrap().ends_with("redirects"),
        "{answer}"
    );

    // In a loop: stopped.
    let answer = fetch(local(&server), &server.url("/loop"), json!({})).await;
    assert_eq!(answer["error"], "TypeError: Too many redirects");
}

#[tokio::test]
async fn secrets_go_to_their_origin_only() {
    let server = Server::start().await;
    let other = Server::start().await;
    let policy = || {
        local(&server)
            .allow(&other.origin)
            .secret(&server.origin, "x-api-key", "host-secret")
    };

    // The host's value, whatever the program sets.
    let init = json!({"headers": {"x-api-key": "the program's"}});
    let answer = fetch(policy(), &server.url("/echo"), init).await;
    let echo: Value = serde_json::from_str(answer["text"].as_str().unwrap()).unwrap();
    assert_eq!(echo["headers"]["x-api-key"], "host-secret");

    // A redirect to another origin takes neither the secret, nor the
    // program's credentials, nor where it came from.
    let init = json!({"headers": {"authorization": "Bearer mine", "cookie": "a=b", "x-keep": "1"}});
    let to_other = server.url(&format!("/redirect/307?{}", other.url("/echo")));
    let answer = fetch(policy(), &to_other, init).await;
    assert_eq!(answer["status"], 200, "{answer}");
    let headers = &other.received()[0].headers;
    assert_eq!(headers.get("x-keep").map(String::as_str), Some("1"));
    for name in ["x-api-key", "authorization", "cookie", "referer"] {
        assert!(!headers.contains_key(name), "{name}: {headers:?}");
    }
}

#[tokio::test]
async fn no_cookies_are_kept_between_requests() {
    let server = Server::start().await;
    let source = r#"
export async function main({ url }) {
  const first = await fetch(url + '/cookie');
  const again = await (await fetch(url + '/echo')).json();
  return { set: first.headers.getSetCookie(), sent: again.headers.cookie ?? null };
}
"#;
    let fetch = Fetch::new(local(&server));
    let answer = run(
        &Store::default(),
        source,
        json!({"url": server.origin}),
        &fetch,
    )
    .await;
    assert_eq!(
        answer["set"],
        json!(["session=s3cret; Path=/", "theme=dark"])
    );
    assert_eq!(answer["sent"], Value::Null);
    // Not by another run either.
    let answer = fetch_echo(&fetch, &server).await;
    assert!(answer["headers"].get("cookie").is_none());
}

async fn fetch_echo(fetch: &Fetch, server: &Server) -> Value {
    let answer = run(
        &Store::default(),
        FETCH,
        json!({"url": server.url("/echo"), "init": {}}),
        fetch,
    )
    .await;
    serde_json::from_str(answer["text"].as_str().unwrap()).unwrap()
}

#[tokio::test]
async fn large_and_slow_responses_fail() {
    let server = Server::start().await;
    let answer = fetch(
        local(&server).max_response(1024),
        &server.url("/big"),
        json!({}),
    )
    .await;
    assert_eq!(
        answer["error"],
        "RangeError: The response is larger than 1024 bytes"
    );
    let policy = local(&server).timeout(Duration::from_millis(200));
    let answer = fetch(policy, &server.url("/slow"), json!({})).await;
    assert_eq!(
        answer["error"],
        "TypeError: fetch failed: the request timed out"
    );
}

#[tokio::test]
async fn a_request_is_sent_again_after_a_crash_as_it_was() {
    let server = Server::start().await;
    let fetch = Fetch::new(local(&server));
    // The program's own idempotency key: random, and in its heap, so the same
    // on every attempt.
    let source = r#"
export async function main({ url }) {
  const key = Math.random().toString(36).slice(2);
  const body = JSON.stringify({ order: 7 });
  const response = await fetch(url, { method: 'POST', headers: { 'idempotency-key': key }, body });
  return { key, status: response.status };
}
"#;
    let store = Store::default();
    let input = json!({"url": server.url("/echo")});
    let catalog = programs(source);
    let mut run = attempt(&store, &catalog, input.clone(), &offered(&fetch)).await;
    let id = run.view().state.pending[0].id().to_owned();
    // Sent; and the attempt crashes before the response commits.
    make(&mut run, &id).await.unwrap();
    drop(run);

    let mut run = attempt(&store, &catalog, input, &offered(&fetch)).await;
    let done = settle(&mut run).await;
    let received = server.received();
    assert_eq!(received.len(), 2);
    assert_eq!(received[0].body, received[1].body);
    assert_eq!(
        received[0].headers["idempotency-key"],
        received[1].headers["idempotency-key"]
    );
    assert_eq!(
        done["key"].as_str(),
        Some(received[0].headers["idempotency-key"].as_str())
    );
}

#[tokio::test]
async fn python_programs_fetch_too() {
    let server = Server::start().await;
    let source = r#"
async def main(input, ctx):
    echo = await ctx.fetch(input["url"] + "/echo", method="put", headers={"X-Trace": "py"}, body=b"\x00\xff")
    cookie = await ctx.fetch(input["url"] + "/cookie")
    data = await ctx.fetch(input["url"] + "/bytes")
    try:
        await ctx.fetch("https://example.com/")
    except Exception as error:
        refused = f"{error.name}: {error}"
    return {
        "status": echo.status_code,
        "echo": echo.json(),
        "cookies": [value for name, value in cookie.header_list if name == "set-cookie"],
        "bytes": list(data.content),
        "refused": refused,
    }
"#;
    let fetch = Fetch::new(local(&server));
    let catalog = catalog(Language::Python, source);
    let answer = drive(
        &Store::default(),
        &catalog,
        json!({"url": server.origin}),
        &fetch,
    )
    .await;
    assert_eq!(answer["status"], 200, "{answer}");
    assert_eq!(answer["echo"]["method"], "PUT");
    assert_eq!(answer["echo"]["headers"]["x-trace"], "py");
    assert_eq!(server.received()[0].body, [0, 255]);
    assert_eq!(
        answer["cookies"],
        json!(["session=s3cret; Path=/", "theme=dark"])
    );
    assert_eq!(answer["bytes"], json!([0, 255, 1, 254]));
    assert_eq!(
        answer["refused"],
        "TypeError: Requests to https://example.com are not allowed"
    );
}
