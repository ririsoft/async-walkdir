// Regression tests for https://github.com/ririsoft/async-walkdir/issues/13
//
// Filtering out many consecutive entries used to overflow the stack of small-stack
// threads (such as tokio workers) because the traversal recursed once per ignored entry.
// A stack overflow aborts the whole process, hence these tests live in their own binary.

use std::path::{Path, PathBuf};

use async_walkdir::{Filtering, WalkDir};
use futures_lite::stream::StreamExt;

// The old recursive code overflows this stack after ~1000-2000 ignored entries in release
// (fewer in debug), so 4000 gives margin. The stack is large enough for the fixed code's
// constant usage on every CI platform (64 KiB was too small on Windows debug builds).
const IGNORED_ENTRIES: usize = 4_000;
const MATCHING_FILES: usize = 5;
const STACK_SIZE: usize = 512 * 1024;

fn run_on_small_stack<F, T>(fut: F) -> T
where
    F: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_stack_size(STACK_SIZE)
        .build()
        .unwrap();
    runtime.block_on(async { tokio::spawn(fut).await.unwrap() })
}

fn sorted(mut v: Vec<PathBuf>) -> Vec<PathBuf> {
    v.sort();
    v
}

async fn collect(mut wd: WalkDir) -> Vec<PathBuf> {
    let mut got = Vec::new();
    while let Some(entry) = wd.next().await {
        got.push(entry.unwrap().path());
    }
    got
}

fn is_m3u8(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext == "m3u8")
}

#[test]
fn filter_ignore_many_files() -> std::io::Result<()> {
    let root = tempfile::tempdir()?;
    // A sub directory ensures popping exhausted directories is covered too.
    let sub = root.path().join("sub");
    std::fs::create_dir(&sub)?;
    let mut want = Vec::new();
    for i in 0..IGNORED_ENTRIES {
        let dir = if i % 2 == 0 { root.path() } else { &sub };
        std::fs::write(dir.join(format!("segment_{i}.ts")), [])?;
    }
    for i in 0..MATCHING_FILES {
        let f = root.path().join(format!("playlist_{i}.m3u8"));
        std::fs::write(&f, [])?;
        want.push(f);
    }

    let wd = WalkDir::new(root.path()).filter(|entry| async move {
        let path = entry.path();
        if path.is_file() && is_m3u8(&path) {
            return Filtering::Continue;
        }
        Filtering::Ignore
    });
    let got = run_on_small_stack(collect(wd));
    assert_eq!(sorted(got), sorted(want));
    Ok(())
}

#[test]
fn filter_ignore_dir_many_dirs() -> std::io::Result<()> {
    let root = tempfile::tempdir()?;
    for i in 0..IGNORED_ENTRIES {
        let d = root.path().join(format!("ignored_{i}"));
        std::fs::create_dir(&d)?;
        std::fs::write(d.join("f.m3u8"), [])?;
    }
    let kept = root.path().join("kept");
    std::fs::create_dir(&kept)?;
    let f = kept.join("f.m3u8");
    std::fs::write(&f, [])?;
    let want = vec![kept, f];

    let wd = WalkDir::new(root.path()).filter(|entry| async move {
        if entry.file_name().to_string_lossy().starts_with("ignored_") {
            return Filtering::IgnoreDir;
        }
        Filtering::Continue
    });
    let got = run_on_small_stack(collect(wd));
    assert_eq!(sorted(got), sorted(want));
    Ok(())
}
