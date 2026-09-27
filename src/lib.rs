// Copyright 2020 Ririsoft <riri@ririsoft.com>
// Copyright 2024 Jordan Danford <jordandanford@gmail.com>
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
//! Symbolic links are walked through but they are not followed.
//!
//! # Example
//!
//! Recursively traverse a directory:
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
//!             }
//!             None => break,
//!         }
//!     }
//! });
//! ```
//!
//! Do not recurse through directories whose name starts with '.':
//!
//! ```
//! use async_walkdir::{Filtering, WalkDir};
//! use futures_lite::future::block_on;
//! use futures_lite::stream::StreamExt;
//!
//! block_on(async {
//!     let mut entries = WalkDir::new("my_directory").filter(|entry| async move {
//!         if let Some(true) = entry
//!             .path()
//!             .file_name()
//!             .map(|f| f.to_string_lossy().starts_with('.'))
//!         {
//!             return Filtering::IgnoreDir;
//!         }
//!         Filtering::Continue
//!     });
//!
//!     loop {
//!         match entries.next().await {
//!             Some(Ok(entry)) => println!("file: {}", entry.path().display()),
//!             Some(Err(e)) => {
//!                 eprintln!("error: {}", e);
//!                 break;
//!             }
//!             None => break,
//!         }
//!     }
//! });
//! ```

#![forbid(unsafe_code)]
#![deny(missing_docs)]

mod error;

use std::future::Future;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};

use async_fs::{read_dir, ReadDir};
use futures_lite::future::Boxed as BoxedFut;
use futures_lite::stream::{self, Stream, StreamExt};

#[doc(no_inline)]
pub use async_fs::DirEntry;

pub use error::Error;
use error::InnerError;

/// A specialized [`Result`][`std::result::Result`] type.
pub type Result<T> = std::result::Result<T, Error>;

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
pub struct WalkDir<State = Unfiltered> {
    root: PathBuf,
    entries: BoxStream,
    opts: WalkDirOptions,
    _state: PhantomData<State>,
}

/// A marker struct to signal, that the DirWalk is not being filtered
pub struct Unfiltered;
/// A marker struct to snignal, that the DirWalk has a filter applied.
/// This results in some options not being configurable after this point anymore.
pub struct Filtered;

#[derive(Default, Clone, Copy)]
struct WalkDirOptions {
    contents_first: bool,
}

/// Sets the filtering behavior.
#[derive(Debug, PartialEq, Eq)]
pub enum Filtering {
    /// Ignore the current entry.
    Ignore,
    /// Ignore the current entry and, if a directory,
    /// do not traverse its childs.
    IgnoreDir,
    /// Continue the normal processing.
    Continue,
}

impl WalkDir {
    /// Returns a new `Walkdir` starting at `root`.
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().to_owned(),
            entries: walk_dir(
                root,
                WalkDirOptions::default(),
                None::<Box<dyn FnMut(DirEntry) -> BoxedFut<Filtering> + Send>>,
            ),
            opts: WalkDirOptions::default(),
            _state: PhantomData,
        }
    }
}
impl WalkDir<Unfiltered> {
    /// Yield the directory's content before the directory.
    ///
    /// When `yes` is false (the default), the directory is yielded before the contents are read.
    /// This is useful when e.g. you want to skip processing some of the directories.
    ///
    /// When `yes` is `true`, the iterator yields the contents of a directory
    /// before yielding the directory itself. This is useful when, e.g. you
    /// want to recursively delete a directory.
    pub fn contents_first(mut self, yes: bool) -> Self {
        let root = self.root.clone();
        self.opts.contents_first = yes;
        Self {
            root: self.root,
            entries: walk_dir(
                root,
                self.opts,
                None::<Box<dyn FnMut(DirEntry) -> BoxedFut<Filtering> + Send>>,
            ),
            opts: self.opts,
            _state: PhantomData,
        }
    }

    /// Filter entries.
    pub fn filter<F, Fut>(self, f: F) -> WalkDir<Filtered>
    where
        F: FnMut(DirEntry) -> Fut + Send + 'static,
        Fut: Future<Output = Filtering> + Send,
    {
        let root = self.root.clone();
        WalkDir {
            root: self.root,
            entries: walk_dir(root, self.opts, Some(f)),
            opts: self.opts,
            _state: PhantomData,
        }
    }
}

impl<State> Stream for WalkDir<State>
where
    State: Unpin,
{
    type Item = Result<DirEntry>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let entries = Pin::new(&mut self.entries);
        entries.poll_next(cx)
    }
}

struct DirStackItem {
    path: PathBuf,
    read_dir: ReadDir,
    // holds the directory, while it is being traversed
    // only relevant when `contents_first` is set to true
    deferred_entry: Option<DirEntry>,
    // when `contents_first` is enabled, the file tree
    // is traversed in post-order, so all non-directory
    // contents of a directory need to be stored in here
    // until the child directories are finished traversing
    deferred_files: Vec<DirEntry>,
}

fn walk_dir<F, Fut>(root: impl AsRef<Path>, opts: WalkDirOptions, filter: Option<F>) -> BoxStream
where
    F: FnMut(DirEntry) -> Fut + Send + 'static,
    Fut: Future<Output = Filtering> + Send,
{
    stream::unfold(
        State::Start((root.as_ref().to_owned(), filter)),
        move |state| async move {
            match state {
                State::Start((root, filter)) => match read_dir(&root).await {
                    Err(source) => Some((
                        Err(InnerError::Io { path: root, source }.into()),
                        State::Done,
                    )),
                    Ok(rd) => {
                        walk(
                            vec![DirStackItem {
                                path: root,
                                read_dir: rd,
                                deferred_files: Vec::new(),
                                deferred_entry: None,
                            }],
                            filter,
                            opts,
                        )
                        .await
                    }
                },
                State::Walk((dirs, filter)) => walk(dirs, filter, opts).await,
                State::Done => None,
            }
        },
    )
    .boxed()
}

enum State<F> {
    Start((PathBuf, Option<F>)),
    Walk((Vec<DirStackItem>, Option<F>)),
    Done,
}

type UnfoldState<F> = (Result<DirEntry>, State<F>);

// Iterative on purpose: filtered-out entries must not grow the stack (see issue #13).
async fn walk<F, Fut>(
    mut dirs: Vec<DirStackItem>,
    mut filter: Option<F>,
    opts: WalkDirOptions,
) -> Option<UnfoldState<F>>
where
    F: FnMut(DirEntry) -> Fut + Send + 'static,
    Fut: Future<Output = Filtering> + Send,
{
    loop {
        let res = {
            let item = dirs.last_mut()?;
            item.read_dir.next().await
        };

        let entry = match res {
            Some(Ok(entry)) => entry,
            Some(Err(source)) => {
                let path = dirs.last().unwrap().path.clone();
                return io_error(path, source, dirs, filter);
            }
            None => {
                let Some(mut popped) = dirs.pop() else {
                    unreachable!()
                };
                if let Some(deferred_file) = popped.deferred_files.pop() {
                    dirs.push(popped);
                    return Some((Ok(deferred_file), State::Walk((dirs, filter))));
                } else if let Some(deferred_dir) = popped.deferred_entry.take() {
                    return Some((Ok(deferred_dir), State::Walk((dirs, filter))));
                } else {
                    continue;
                }
            }
        };

        let ft = match entry.file_type().await {
            Ok(ft) => ft,
            Err(source) => return io_error(entry.path(), source, dirs, filter),
        };

        let filtering = match filter.as_mut() {
            Some(filter) => filter(entry.clone()).await,
            None => Filtering::Continue,
        };

        match filtering {
            Filtering::IgnoreDir => continue,
            Filtering::Ignore => {
                if ft.is_dir() {
                    let path = entry.path();
                    let rd = match read_dir(&path).await {
                        Ok(rd) => rd,
                        Err(source) => return io_error(path, source, dirs, filter),
                    };
                    dirs.push(DirStackItem {
                        path,
                        read_dir: rd,
                        deferred_entry: None,
                        deferred_files: Vec::new(),
                    });
                }
                continue;
            }
            Filtering::Continue => {
                if ft.is_dir() {
                    let path = entry.path();
                    let rd = match read_dir(&path).await {
                        Ok(rd) => rd,
                        Err(source) => return io_error(path, source, dirs, filter),
                    };

                    let deferred_entry = if opts.contents_first {
                        Some(entry.clone())
                    } else {
                        None
                    };

                    dirs.push(DirStackItem {
                        path,
                        read_dir: rd,
                        deferred_entry,
                        deferred_files: Vec::new(),
                    });

                    if opts.contents_first {
                        continue;
                    } else {
                        return Some((Ok(entry), State::Walk((dirs, filter))));
                    }
                } else {
                    if opts.contents_first {
                        if let Some(last) = dirs.last_mut() {
                            last.deferred_files.push(entry);
                        }
                        continue;
                    } else {
                        return Some((Ok(entry), State::Walk((dirs, filter))));
                    }
                }
            }
        }
    }
}

fn io_error<F>(
    path: PathBuf,
    source: std::io::Error,
    dirs: Vec<DirStackItem>,
    filter: Option<F>,
) -> Option<UnfoldState<F>> {
    let err = InnerError::Io { path, source }.into();
    Some((Err(err), State::Walk((dirs, filter))))
}

#[cfg(test)]
mod tests {
    use std::io::{ErrorKind, Result};

    use futures_lite::future::block_on;
    use futures_lite::stream::StreamExt;

    use super::{Filtering, WalkDir};

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
                Err(e) => {
                    assert_eq!(wd.root, e.path().unwrap());
                    assert_eq!(e.io().unwrap().kind(), ErrorKind::NotFound);
                    assert_eq!(e.into_io().unwrap().kind(), ErrorKind::NotFound);
                }
                _ => panic!("want IO error"),
            }
            assert!(wd.next().await.is_none());
        })
    }

    #[test]
    fn walk_dir_read_dir_error() -> Result<()> {
        block_on(async {
            let root = tempfile::tempdir()?;
            let d1 = root.path().join("d1");
            async_fs::create_dir_all(&d1).await?;
            // Removing the directory from the filter makes the subsequent read_dir fail.
            let mut wd = WalkDir::new(root.path()).filter(|entry| async move {
                async_fs::remove_dir(entry.path()).await.unwrap();
                Filtering::Continue
            });
            match wd.next().await.unwrap() {
                Err(e) => {
                    assert_eq!(e.path().unwrap(), d1.as_path());
                    assert_eq!(e.io().unwrap().kind(), ErrorKind::NotFound);
                }
                _ => panic!("want IO error"),
            }
            assert!(wd.next().await.is_none());
            Ok(())
        })
    }

    #[test]
    fn into_io_error() {
        block_on(async {
            let mut wd = WalkDir::new("foobar");
            match wd.next().await.unwrap() {
                Err(e) => {
                    let e: std::io::Error = e.into();
                    assert_eq!(e.kind(), ErrorKind::NotFound);
                }
                _ => panic!("want IO error"),
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

    #[test]
    fn filter_dirs() -> Result<()> {
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

            let want = vec![f3.to_owned(), f2.to_owned(), f1.to_owned()];

            let mut wd = WalkDir::new(root.path()).filter(|entry| async move {
                match entry.file_type().await {
                    Ok(ft) if ft.is_dir() => Filtering::Ignore,
                    _ => Filtering::Continue,
                }
            });

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
    #[test]
    fn contents_first() -> Result<()> {
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
                f3.to_owned(),
                d2.to_owned(),
                f2.to_owned(),
                d1.to_owned(),
                f1.to_owned(),
            ];
            let mut wd = WalkDir::new(root.path()).contents_first(true);

            let mut got = Vec::new();
            while let Some(entry) = wd.next().await {
                let entry = entry.unwrap();
                got.push(entry.path());
            }
            assert_eq!(got, want);

            Ok(())
        })
    }

    #[test]
    fn filter_dirs_no_traverse() -> Result<()> {
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

            let want = vec![d1, f2.to_owned(), f1.to_owned()];

            let mut wd = WalkDir::new(root.path()).filter(move |entry| {
                let d2 = d2.clone();
                async move {
                    if entry.path() == d2 {
                        Filtering::IgnoreDir
                    } else {
                        Filtering::Continue
                    }
                }
            });

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

#[cfg(all(unix, test))]
mod test_unix {
    use async_fs::unix::PermissionsExt;
    use std::io::Result;

    use futures_lite::future::block_on;
    use futures_lite::stream::StreamExt;

    use super::{Filtering, WalkDir};

    #[test]
    fn filter_ignore_dir_does_not_read_it() -> Result<()> {
        block_on(async {
            let root = tempfile::tempdir()?;
            let d1 = root.path().join("d1");
            async_fs::create_dir_all(&d1).await?;
            let mut perms = async_fs::metadata(&d1).await?.permissions();
            perms.set_mode(0o222);
            async_fs::set_permissions(&d1, perms).await?;
            let mut wd = WalkDir::new(&root).filter(|_| async { Filtering::IgnoreDir });
            assert!(wd.next().await.is_none());
            Ok(())
        })
    }

    #[test]
    fn walk_dir_error_path() -> Result<()> {
        block_on(async {
            let root = tempfile::tempdir()?;
            let d1 = root.path().join("d1");
            async_fs::create_dir_all(&d1).await?;
            let mut perms = async_fs::metadata(&d1).await?.permissions();
            perms.set_mode(0o222);
            async_fs::set_permissions(&d1, perms).await?;
            let mut wd = WalkDir::new(&root);
            match wd.next().await.unwrap() {
                Err(e) => assert_eq!(e.path().unwrap(), d1.as_path()),
                _ => panic!("want IO error"),
            }
            Ok(())
        })
    }
}
