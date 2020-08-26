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
//! # Example
//!
//! ```
//! use async_walkdir::WalkDir;
//! use futures_lite::future::block_on;
//! use futures_lite::stream::StreamExt;
//!
//! block_on(async {
//!     let mut entries = WalkDir::new("my_directory").await?;
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
//! })?;
//! # Ok::<(), std::io::Error>(())
//! ```

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};

use async_fs::read_dir;
use futures_lite::future::{Boxed as BoxedFut, FutureExt};
use futures_lite::stream::{self, Stream, StreamExt};

pub use async_fs::DirEntry;
pub use futures_lite::stream::Boxed as BoxedStream;
pub use std::io::Result;

/// A `Stream` of `DirEntry` generated from recursively traversing
/// a directory.
///
/// Entries are returned without a specific ordering.
///
/// # Panics
///
/// Panics if the directories depth overflows `usize`.
pub struct WalkDir {
    entries: BoxedStream<Result<DirEntry>>,
}

impl WalkDir {
    /// Returns a new `Walkdir` starting at `root`.
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            entries: walk_dir_internal(root),
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

fn walk_dir_internal(root: impl AsRef<Path>) -> BoxedStream<Result<DirEntry>> {
    stream::unfold(State::Start(root.as_ref().to_owned()), |state| async move {
        match state {
            State::Start(root) => match read_dir(root).await {
                Err(e) => return Some((Err(e), State::Done)),
                Ok(rd) => return walk(vec![rd.boxed()]).await,
            },
            State::Walk(dirs) => return walk(dirs).await,
            State::Done => return None,
        }
    })
    .boxed()
}

enum State {
    Start(PathBuf),
    Walk(Vec<BoxedStream<Result<DirEntry>>>),
    Done,
}

fn walk(
    mut dirs: Vec<BoxedStream<Result<DirEntry>>>,
) -> BoxedFut<Option<(Result<DirEntry>, State)>> {
    async move {
        if let Some(dir) = dirs.last_mut() {
            match dir.next().await {
                Some(Ok(entry)) => match entry.file_type().await {
                    Err(e) => return Some((Err(e), State::Walk(dirs))),
                    Ok(ft) if ft.is_dir() => {
                        let wd = walk_dir_internal(entry.path());
                        dirs.push(wd);
                        return walk(dirs).await;
                    }
                    Ok(_) => return Some((Ok(entry), State::Walk(dirs))),
                },
                Some(Err(e)) => return Some((Err(e), State::Walk(dirs))),
                None => {
                    dirs.pop();
                    return walk(dirs).await;
                }
            }
        }
        None
    }
    .boxed()
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

            let mut got = Vec::new();
            let want = vec![f3.to_owned(), f2.to_owned(), f1.to_owned()];
            let mut wd = WalkDir::new(root.path());

            let next = wd.next().await.unwrap()?;
            got.push(next.path());

            let next = wd.next().await.unwrap()?;
            got.push(next.path());

            let next = wd.next().await.unwrap()?;
            got.push(next.path());

            got.sort();
            assert_eq!(got, want);

            assert!(wd.next().await.is_none());
            Ok(())
        })
    }
}
