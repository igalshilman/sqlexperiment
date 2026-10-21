//! `ctx.fs`, the extension `fs`: a scope's files, checkpointed with its
//! commits into blobs in a directory, and commands run on a checkpoint.
mod common;

use common::{Store, attempt, make, programs, settle};
use functions::{Blobs, DirBlobs, Extensions, Files, Operation, Outcome, Run};
use serde_json::{Value, json};
use tempfile::TempDir;

/// The extension `fs`, the scopes' blobs in a new directory.
fn offered() -> (Extensions, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let extensions =
        (Extensions::new().with_blobs(DirBlobs::new(dir.path()))).with("fs", Files::new());
    (extensions, dir)
}

/// The blobs of program "v1"'s object "s".
async fn blobs(dir: &TempDir) -> Vec<String> {
    let mut names = DirBlobs::new(dir.path()).list("v1/s").await.unwrap();
    names.sort();
    names
}

const NOTES: &str = r#"
export async function main({ write }, ctx) {
  if (write) {
    ctx.fs.write('notes/a.txt', 'alpha');
    ctx.fs.write('./notes//b.txt', 'beta');
    await ctx.sleep(10);
  }
  return { paths: ctx.fs.list(), a: ctx.fs.read('notes/a.txt'), missing: ctx.fs.read('nope') };
}
"#;

#[tokio::test]
async fn files_are_saved_with_the_commit_and_stay_from_run_to_run() {
    let (extensions, dir) = offered();
    let store = Store::default();
    let catalog = programs(NOTES);
    // The first turn writes, and commits: the commit holds the manifest, the
    // files' blobs written before it.
    let mut run = attempt(&store, &catalog, json!({"write": true}), &extensions).await;
    let manifest = store.record("fs/manifest");
    let blob = manifest["notes/a.txt"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(blobs(&dir).await.contains(&blob), "{manifest}");
    drop(run);

    // Another attempt: the files as the commit left them.
    run = attempt(&store, &catalog, json!({"write": true}), &extensions).await;
    let done = settle(&mut run).await;
    let expected = json!({"paths": ["notes/a.txt", "notes/b.txt"], "a": "alpha", "missing": null});
    assert_eq!(done, expected);

    // The next run in the scope: the same files.
    let scope = store.scope().extended(extensions.clone());
    let next = Run::start(&catalog, &scope, "s", "r2", "v1", "main", json!({}), 20)
        .await
        .unwrap();
    assert_eq!(common::value(&next.view()), expected);
}

#[tokio::test]
async fn a_command_runs_on_the_checkpoint_and_its_changes_join_the_files() {
    let (extensions, _dir) = offered();
    let source = r#"
export async function main(_, ctx) {
  ctx.fs.write('count.txt', '1');
  ctx.fs.write('src/gone.txt', 'bye');
  const ran = await ctx.fs.exec(
    'echo $(( $(cat count.txt) + 1 )) > count.txt; mkdir -p out; echo made > out/new.txt; rm src/gone.txt; cat count.txt',
  );
  return {
    ran,
    count: ctx.fs.read('count.txt'),
    made: ctx.fs.read('out/new.txt'),
    gone: ctx.fs.read('src/gone.txt'),
    paths: ctx.fs.list(),
  };
}
"#;
    let done = common::drive(
        &Store::default(),
        &programs(source),
        Value::Null,
        &extensions,
    )
    .await;
    assert_eq!(
        done,
        json!({
            // Their paths: the contents are in the files, not the heap.
            "ran": {"exit": 0, "output": "2\n", "changed": ["count.txt", "out/new.txt", "src/gone.txt"]},
            "count": "2\n",
            "made": "made\n",
            "gone": null,
            "paths": ["count.txt", "out/new.txt"],
        })
    );
}

#[tokio::test]
async fn a_command_runs_again_after_a_crash_on_the_same_files() {
    let (extensions, _dir) = offered();
    let source = r#"
export async function main(_, ctx) {
  ctx.fs.write('log.txt', 'start\n');
  await ctx.fs.exec('echo ran >> log.txt');
  return ctx.fs.read('log.txt');
}
"#;
    let store = Store::default();
    let catalog = programs(source);
    // The command runs, and the attempt crashes before its outcome commits.
    let mut run = attempt(&store, &catalog, Value::Null, &extensions).await;
    let id = run.view().state.pending[0].id().to_owned();
    make(&mut run, &id).await.unwrap();
    drop(run);

    // It runs again, on the same checkpoint: what it did, it did once.
    let mut run = attempt(&store, &catalog, Value::Null, &extensions).await;
    assert_eq!(settle(&mut run).await, json!("start\nran\n"));
}

#[tokio::test]
async fn files_cannot_change_while_a_command_runs() {
    let (extensions, _dir) = offered();
    let source = r#"
export async function main(_, ctx) {
  ctx.fs.write('a.txt', 'before');
  const command = ctx.fs.exec('cat a.txt');
  await ctx.sleep(5);
  let refused;
  try {
    ctx.fs.write('a.txt', 'meanwhile');
  } catch (error) {
    refused = error.message;
  }
  const second = await ctx.fs.exec('true').catch((error) => error.message);
  const ran = await command;
  ctx.fs.write('a.txt', 'after');
  return { refused, second, output: ran.output, a: ctx.fs.read('a.txt') };
}
"#;
    let store = Store::default();
    let mut run = attempt(&store, &programs(source), Value::Null, &extensions).await;
    // The timer first, while the command is still pending.
    let view = run.view();
    let Some(Operation::Sleep { id, deadline }) = view.state.pending.get(1).cloned() else {
        panic!("Expected a timer: {:?}", view.state.pending);
    };
    run.deliver(&id, Outcome::success(Value::Null), deadline)
        .await
        .unwrap();
    assert_eq!(
        settle(&mut run).await,
        json!({
            "refused": "The files cannot change while a command runs",
            "second": "A command is already running",
            "output": "before",
            "a": "after",
        })
    );
}

#[tokio::test]
async fn a_run_that_ends_with_a_command_running_frees_the_files() {
    let (extensions, _dir) = offered();
    let source = r#"
export async function main({ leave }, ctx) {
  ctx.fs.write('a.txt', leave ? 'left' : 'next');
  if (leave) {
    ctx.fs.exec('true');
    await ctx.sleep(5);
    return 'left';
  }
  return (await ctx.fs.exec('cat a.txt')).output;
}
"#;
    let store = Store::default();
    let catalog = programs(source);
    // The command is issued; the run ends on the timer, before its outcome.
    let mut run = attempt(&store, &catalog, json!({"leave": true}), &extensions).await;
    let view = run.view();
    let Some(Operation::Sleep { id, deadline }) = view.state.pending.get(1).cloned() else {
        panic!("Expected a timer: {:?}", view.state.pending);
    };
    run.deliver(&id, Outcome::success(Value::Null), deadline)
        .await
        .unwrap();
    assert_eq!(common::value(&run.view()), json!("left"));

    // The next run in the scope may change the files and run a command.
    let scope = store.scope().extended(extensions.clone());
    let mut next = Run::start(&catalog, &scope, "s", "r2", "v1", "main", json!({}), 20)
        .await
        .unwrap();
    assert_eq!(settle(&mut next).await, json!("next"));
}

#[tokio::test]
async fn blobs_no_commit_names_are_deleted() {
    let (extensions, dir) = offered();
    let source = r#"
export async function main(_, ctx) {
  ctx.fs.write('a.txt', 'one');
  ctx.fs.write('b.txt', 'kept');
  await ctx.sleep(5);
  ctx.fs.write('a.txt', 'two');
  ctx.fs.remove('b.txt');
  await ctx.sleep(5);
  return ctx.fs.list();
}
"#;
    let done = common::drive(
        &Store::default(),
        &programs(source),
        Value::Null,
        &extensions,
    )
    .await;
    assert_eq!(done, json!(["a.txt"]));
    // "two", the only file left: nothing else.
    assert_eq!(blobs(&dir).await.len(), 1, "{:?}", blobs(&dir).await);
}

#[tokio::test]
async fn paths_stay_inside_the_files() {
    let (extensions, _dir) = offered();
    let source = r#"
export async function main(_, ctx) {
  const errors = [];
  for (const path of ['../up.txt', '/etc/passwd', 'a/../../b', '', '.']) {
    try {
      ctx.fs.write(path, 'x');
    } catch (error) {
      errors.push(error.name);
    }
  }
  return errors;
}
"#;
    let done = common::drive(
        &Store::default(),
        &programs(source),
        Value::Null,
        &extensions,
    )
    .await;
    assert_eq!(done, json!(vec!["TypeError"; 5]));
}
