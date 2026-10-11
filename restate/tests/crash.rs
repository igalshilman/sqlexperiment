//! Crash campaign against a real Restate server: the ledger aborts at random
//! state requests and the server is killed three times, while clients send transfers
//! (each resent until answered). The audit must find every transfer applied
//! exactly once and the balances consistent with them.
//!
//! Needs `RESTATE_SERVER` (a `restate-server` binary); skipped otherwise.
//! `LEDGER_COMMAND` replaces the Rust ledger, e.g. `node ../node/restate/ledger.mjs`.
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const INGRESS: &str = "http://127.0.0.1:8080";
const ADMIN: &str = "http://127.0.0.1:9070";
const LEDGER_PORT: u16 = 9181;
const KEYS: [&str; 2] = ["a", "b"];
const ACCOUNTS: i64 = 8;
const OPENING: i64 = 1000;
const TRANSFERS_PER_KEY: usize = 60;
const CLIENTS_PER_KEY: usize = 3;
const SERVER_KILLS: usize = 3;

fn log(path: &Path) -> std::fs::File {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap()
}

fn server(binary: &Path, dir: &Path) -> Child {
    Command::new(binary)
        .args(["--base-dir", dir.join("server").to_str().unwrap()])
        .env("RESTATE_BIND_IP", "127.0.0.1")
        .env("RESTATE_LISTEN_MODE", "tcp")
        .stdout(log(&dir.join("server.log")))
        .stderr(log(&dir.join("server.log")))
        .spawn()
        .expect("start restate-server")
}

fn wait_healthy() {
    let deadline = Instant::now() + Duration::from_secs(60);
    while ureq::get(&format!("{ADMIN}/health")).call().is_err() {
        assert!(Instant::now() < deadline, "restate-server did not start");
        thread::sleep(Duration::from_millis(200));
    }
}

/// Keep a ledger running, restarting it whenever it aborts.
fn supervise(dir: PathBuf, per_mille: u32, stop: Arc<AtomicBool>, starts: Arc<AtomicUsize>) {
    while !stop.load(Ordering::SeqCst) {
        // `LEDGER_COMMAND` runs another port of the ledger, such as the Node.js one.
        let command = std::env::var("LEDGER_COMMAND")
            .unwrap_or_else(|_| env!("CARGO_BIN_EXE_ledger").to_owned());
        let mut words = command.split_whitespace();
        let mut child = Command::new(words.next().expect("a ledger command"))
            .args(words)
            .env("PORT", LEDGER_PORT.to_string())
            .env("LEDGER_ABORT_PER_MILLE", per_mille.to_string())
            .env("SQLITE_KV_CACHE_FILE", dir.join("cache"))
            .stdout(Stdio::null())
            .stderr(log(&dir.join("ledger.log")))
            .spawn()
            .expect("start ledger");
        starts.fetch_add(1, Ordering::SeqCst);
        loop {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            if stop.load(Ordering::SeqCst) {
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}

/// POST until the call is answered with 200, through crashes of either side.
fn call(path: &str, body: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        let request = ureq::post(&format!("{INGRESS}/{path}")).timeout(Duration::from_secs(30));
        let result = if body.is_empty() {
            request.call()
        } else {
            request
                .set("content-type", "application/json")
                .send_string(body)
        };
        match result {
            Ok(response) => {
                let mut text = String::new();
                response.into_reader().read_to_string(&mut text).unwrap();
                return text;
            }
            Err(ureq::Error::Status(code, response)) if code != 404 => {
                let body = response.into_string().unwrap_or_default();
                // A failed invocation, unlike an unavailable server, is final.
                if code < 500 || body.contains(r#""source":"invocation""#) {
                    panic!("{path}: {code} {body}");
                }
                assert!(Instant::now() < deadline, "{path}: no answer");
                thread::sleep(Duration::from_millis(100));
            }
            Err(_) => {
                assert!(Instant::now() < deadline, "{path}: no answer");
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

fn number(json: &str, field: &str) -> i64 {
    let start = json.find(&format!("\"{field}\":")).expect(field) + field.len() + 3;
    let digits: String = json[start..]
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '-')
        .collect();
    digits.parse().unwrap()
}

#[test]
fn crash_campaign() {
    let Some(binary) = std::env::var_os("RESTATE_SERVER").map(PathBuf::from) else {
        eprintln!("RESTATE_SERVER is not set; skipping");
        return;
    };
    let dir = std::env::temp_dir().join(format!("sqlite-kv-restate-{}", std::process::id()));
    eprintln!("logs in {}", dir.display());
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut restate = server(&binary, &dir);
    wait_healthy();

    let stop = Arc::new(AtomicBool::new(false));
    let starts = Arc::new(AtomicUsize::new(0));
    let per_mille: u32 = std::env::var("ABORT_PER_MILLE").map_or(15, |v| v.parse().unwrap());
    let supervisor = {
        let (dir, stop, starts) = (dir.clone(), stop.clone(), starts.clone());
        thread::spawn(move || supervise(dir, per_mille, stop, starts))
    };
    let deadline = Instant::now() + Duration::from_secs(60);
    while ureq::post(&format!("{ADMIN}/deployments"))
        .set("content-type", "application/json")
        .send_string(&format!(
            r#"{{"uri":"http://127.0.0.1:{LEDGER_PORT}","force":true}}"#
        ))
        .is_err()
    {
        assert!(Instant::now() < deadline, "registration failed");
        thread::sleep(Duration::from_millis(200));
    }
    for key in KEYS {
        call(
            &format!("Ledger/{key}/init"),
            &format!("[{ACCOUNTS}, {OPENING}]"),
        );
    }

    // Clients send transfers; one in three is sent twice.
    let applied = Arc::new(AtomicUsize::new(0));
    let sent = Arc::new(AtomicUsize::new(0));
    let mut clients = Vec::new();
    for key in KEYS {
        for client in 0..CLIENTS_PER_KEY {
            let (applied, sent) = (applied.clone(), sent.clone());
            clients.push(thread::spawn(move || {
                let mut x =
                    (client as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ key.len() as u64;
                for n in (client..TRANSFERS_PER_KEY).step_by(CLIENTS_PER_KEY) {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    let (from, to) = (x % ACCOUNTS as u64, (x >> 8) % ACCOUNTS as u64);
                    let body = format!(
                        r#"{{"id":"{key}-{n}","from":{from},"to":{to},"amount":{}}}"#,
                        1 + (x >> 16) % 50
                    );
                    let times = if n % 3 == 0 { 2 } else { 1 };
                    for _ in 0..times {
                        if call(&format!("Ledger/{key}/transfer"), &body) == "true" {
                            applied.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                    let sent = sent.fetch_add(1, Ordering::SeqCst) + 1;
                    if sent % 20 == 0 {
                        eprintln!("{sent} transfers answered");
                    }
                }
            }));
        }
    }
    let total = KEYS.len() * TRANSFERS_PER_KEY;
    // Killing the server loses writes of attempts in flight while the ledger,
    // and its page cache, live on.
    for kill in 1..=SERVER_KILLS {
        // A failed client stops early: its panic surfaces when it is joined.
        while sent.load(Ordering::SeqCst) < total * kill / (SERVER_KILLS + 1)
            && !clients.iter().all(|c| c.is_finished())
        {
            thread::sleep(Duration::from_millis(20));
        }
        eprintln!(
            "killing the server after {} transfers",
            sent.load(Ordering::SeqCst)
        );
        restate.kill().unwrap();
        restate.wait().unwrap();
        restate = server(&binary, &dir);
        wait_healthy();
    }
    for client in clients {
        client.join().unwrap();
    }

    // Stop the faults and audit.
    stop.store(true, Ordering::SeqCst);
    supervisor.join().unwrap();
    let ledger_starts = starts.load(Ordering::SeqCst);
    let stop = Arc::new(AtomicBool::new(false));
    let supervisor = {
        let (dir, stop) = (dir.clone(), stop.clone());
        thread::spawn(move || supervise(dir, 0, stop, Arc::default()))
    };
    for key in KEYS {
        let audit = call(&format!("Ledger/{key}/audit"), "");
        eprintln!("{key}: {audit}");
        assert_eq!(number(&audit, "accounts"), ACCOUNTS, "{key}");
        assert_eq!(number(&audit, "total"), ACCOUNTS * OPENING, "{key}");
        assert_eq!(
            number(&audit, "transfers"),
            TRANSFERS_PER_KEY as i64,
            "{key}"
        );
        assert_eq!(number(&audit, "inconsistent"), 0, "{key}");
    }
    eprintln!(
        "{total} transfers, {} answered as applied, ledger started {ledger_starts} times",
        applied.load(Ordering::SeqCst)
    );
    // An answer can be lost to a crash, and the resend then finds it applied.
    assert!(applied.load(Ordering::SeqCst) <= total);
    assert!(ledger_starts > 1, "no abort happened");

    stop.store(true, Ordering::SeqCst);
    supervisor.join().unwrap();
    restate.kill().unwrap();
    restate.wait().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
