// Copyright 2020 Ririsoft <riri@ririsoft.com>
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! An utility for walking through a directory asynchronously and recursively.
//!
//! Based on [async-fs](https://docs.rs/async-fs) and [blocking](https://docs.rs/blocking),
//! it uses a thread pool to handle blocking IOs. Please refere to those crates for the rationale.
//! This crate is compatible with any async runtime based on [futures 0.3](https://docs.rs/futures-core),
//! which includes [tokio](https://docs.rs/tokio), [async-std](https://docs.rs/async-std) and [smol](https://docs.rs/smol).
//!
//! # Example
//!
//! ```
//! use async_walkdir::WalkDir;
//! use futures_lite::future::block_on;
//! use futures_lite::stream::StreamExt;
//!
//! block_on(async {
//!     let mut entries = WalkDir::new("my_directory");
//!     loop {
//!         match entries.next().await {
//!             Some(Ok(entry)) => println!("file: {}", entry.path().display()),
//!             Some(Err(e)) => {
//!                 eprintln!("error: {}", e);
//!                 break;
//!             },
//!             None => break,
//!         }
//!     }
//! });
//! ```

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};

use async_fs::{read_dir, ReadDir};
use futures_lite::future::{Boxed as BoxedFut, FutureExt};
use futures_lite::stream::{self, Stream, StreamExt};

#[doc(no_inline)]
pub use async_fs::DirEntry;
#[doc(no_inline)]
pub use std::io::Result;

type BoxStream = futures_lite::stream::Boxed<Result<DirEntry>>;

/// A `Stream` of `DirEntry` generated from recursively traversing
/// a directory.
///
/// Entries are returned without a specific ordering. The top most root directory
/// is not returned but child directories are.
///
/// # Panics
///
/// Panics if the directories depth overflows `usize`.
pub struct WalkDir {
    entries: BoxStream,
}

impl WalkDir {
    /// Returns a new `Walkdir` starting at `root`.
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            entries: walk_dir(root),
        }
    }
}

impl Stream for WalkDir {
    type Item = Result<DirEntry>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let entries = Pin::new(&mut self.entries);
        entries.poll_next(cx)
    }
}

fn walk_dir(root: impl AsRef<Path>) -> BoxStream {
    stream::unfold(State::Start(root.as_ref().to_owned()), |state| async move {
        match state {
            State::Start(root) => match read_dir(root).await {
                Err(e) => return Some((Err(e), State::Done)),
                Ok(rd) => return walk(vec![rd]).await,
            },
            State::Walk(dirs) => return walk(dirs).await,
            State::Done => return None,
        }
    })
    .boxed()
}

enum State {
    Start(PathBuf),
    Walk(Vec<ReadDir>),
    Done,
}

type UnfoldState = Option<(Result<DirEntry>, State)>;

fn walk(mut dirs: Vec<ReadDir>) -> BoxedFut<UnfoldState> {
    async move {
        if let Some(dir) = dirs.last_mut() {
            match dir.next().await {
                Some(Ok(entry)) => walk_entry(entry, dirs).await,
                Some(Err(e)) => Some((Err(e), State::Walk(dirs))),
                None => {
                    dirs.pop();
                    walk(dirs).await
                }
            }
        } else {
            None
        }
    }
    .boxed()
}

async fn walk_entry(entry: DirEntry, mut dirs: Vec<ReadDir>) -> UnfoldState {
    match entry.file_type().await {
        Err(e) => Some((Err(e), State::Walk(dirs))),
        Ok(ft) if ft.is_dir() => {
            let rd = match read_dir(entry.path()).await {
                Err(e) => return Some((Err(e), State::Done)),
                Ok(rd) => rd,
            };
            dirs.push(rd);
            Some((Ok(entry), State::Walk(dirs)))
        }
        Ok(_) => Some((Ok(entry), State::Walk(dirs))),
    }
}

#[cfg(test)]
mod tests {
    use super::WalkDir;
    use futures_lite::future::block_on;
    use futures_lite::stream::StreamExt;
    use std::io::{ErrorKind, Result};

    #[test]
    fn walk_dir_empty() -> Result<()> {
        block_on(async {
            let root = tempfile::tempdir()?;
            let mut wd = WalkDir::new(root.path());
            assert!(wd.next().await.is_none());
            Ok(())
        })
    }

    #[test]
    fn walk_dir_not_exist() {
        block_on(async {
            let mut wd = WalkDir::new("foobar");
            match wd.next().await.unwrap() {
                Ok(_) => panic!("want error"),
                Err(e) => assert_eq!(e.kind(), ErrorKind::NotFound),
            }
        })
    }

    #[test]
    fn walk_dir_files() -> Result<()> {
        block_on(async {
            let root = tempfile::tempdir()?;
            let f1 = root.path().join("f1.txt");
            let d1 = root.path().join("d1");
            let f2 = d1.join("f2.txt");
            let d2 = d1.join("d2");
            let f3 = d2.join("f3.txt");

            async_fs::create_dir_all(&d2).await?;
            async_fs::write(&f1, []).await?;
            async_fs::write(&f2, []).await?;
            async_fs::write(&f3, []).await?;

            let want = vec![
                d1.to_owned(),
                d2.to_owned(),
                f3.to_owned(),
                f2.to_owned(),
                f1.to_owned(),
            ];
            let mut wd = WalkDir::new(root.path());

            let mut got = Vec::new();
            while let Some(entry) = wd.next().await {
                let entry = entry.unwrap();
                got.push(entry.path());
            }
            got.sort();
            assert_eq!(got, want);

            Ok(())
        })
    }
}
