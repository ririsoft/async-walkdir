//! A directory walker that never follows symbolic links, even when the
//! tree is modified concurrently.
//!
//! [`crate::WalkDir`] checks each entry, then opens directories again by
//! path. A concurrent process can swap a directory for a symbolic link
//! between those two steps and make the walk escape the root directory
//! (see the crate level [security notes](crate#security)).
//!
//! The [`WalkDir`] of this module walks with directory *handles* instead of
//! paths:
//!
//! - the root directory is opened once, then every child directory is opened
//!   *relative to its already opened parent* with symlink following disabled
//!   (`openat(2)` with `O_NOFOLLOW` on Unix, `NtCreateFile` with a root
//!   directory handle and `FILE_OPEN_REPARSE_POINT` on Windows);
//! - metadata is read relative to the parent directory handle, without
//!   following symbolic links;
//! - a directory swapped for a symbolic link (or a junction on Windows)
//!   between the listing and the opening is detected: the walker yields an error for that entry, does not
//!   descend into it, and continues with the next entries;
//! - swapping or moving a *parent* directory has no effect: the walker keeps
//!   reading the directories it has opened, wherever they are moved.
//!
//! Each [`DirEntry`] keeps its parent directory handle alive and exposes it,
//! so that you can act on the entry without resolving its path again, which
//! would reintroduce the race in your own code. [`DirEntry::open`] opens a
//! file that way.
//!
//! # Limitations
//!
//! - The root path given to [`WalkDir::new`] is resolved once, following
//!   symbolic links: it must be trusted. Use `WalkDir::from_fd` (Unix) or
//!   `WalkDir::from_handle` (Windows) to start from a directory handle you
//!   already hold.
//! - [`DirEntry::path`] is informational: it is the path the entry had when
//!   it was listed. Acting on it resolves it again and is subject to races.
//! - Each directory currently being walked keeps two handles open, and each
//!   yielded [`DirEntry`] keeps its parent directory open until it is
//!   dropped.
//! - On Windows, [`DirEntry::metadata`] returns the metadata found while
//!   listing the parent directory, like [`std::fs::DirEntry::metadata`].
//!
//! # Example
//!
//! ```
//! use async_walkdir::secure::WalkDir;
//! use futures_lite::future::block_on;
//! use futures_lite::io::AsyncReadExt;
//! use futures_lite::stream::StreamExt;
//!
//! block_on(async {
//!     let mut entries = WalkDir::new("my_directory");
//!     while let Some(entry) = entries.next().await {
//!         let entry = match entry {
//!             Ok(entry) => entry,
//!             Err(e) => {
//!                 eprintln!("error: {}", e);
//!                 continue;
//!             }
//!         };
//!         if entry.file_type().await.map(|ft| ft.is_file()).unwrap_or(false) {
//!             // Opened relative to its parent directory, never following symlinks.
//!             let mut content = String::new();
//!             entry.open().await?.read_to_string(&mut content).await?;
//!         }
//!     }
//!     std::io::Result::Ok(())
//! });
//! ```

use std::collections::VecDeque;
use std::ffi::OsString;
use std::fmt;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::SystemTime;

use blocking::unblock;
use futures_lite::future::Boxed as BoxedFut;
use futures_lite::stream::{self, Stream, StreamExt};

use crate::error::InnerError;
use crate::{Filtering, Result};

#[cfg(unix)]
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
#[cfg(windows)]
use std::os::windows::io::{AsHandle, BorrowedHandle, OwnedHandle};

#[cfg(unix)]
#[path = "unix.rs"]
mod sys;
#[cfg(windows)]
#[path = "windows.rs"]
mod sys;

#[cfg(test)]
mod tests;

/// Number of entries read per round trip to the blocking thread pool.
const BATCH_SIZE: usize = 32;

type BoxStream = futures_lite::stream::Boxed<Result<DirEntry>>;

/// A `Stream` of [`DirEntry`] generated from recursively traversing a
/// directory without ever following symbolic links.
///
/// This is the race-free counterpart of [`crate::WalkDir`], with the same
/// behavior otherwise: entries are returned without a specific ordering, the
/// root directory itself is not returned, and IO errors are yielded without
/// stopping the walk.
pub struct WalkDir {
    root: Root,
    entries: BoxStream,
}

#[derive(Clone)]
enum Root {
    Path(PathBuf),
    Handle(Arc<sys::Handle>),
}

impl WalkDir {
    /// Returns a new `WalkDir` starting at `root`.
    ///
    /// `root` is resolved once when the walk starts, following symbolic
    /// links: it must be trusted.
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self::with_root(Root::Path(root.as_ref().to_owned()))
    }

    /// Returns a new `WalkDir` starting at the already opened directory `dir`.
    ///
    /// No path is resolved at all. The paths of the yielded entries are
    /// relative to `dir`, for instance `sub/file.txt`.
    #[cfg(unix)]
    pub fn from_fd(dir: OwnedFd) -> Self {
        Self::with_root(Root::Handle(Arc::new(dir)))
    }

    /// Returns a new `WalkDir` starting at the already opened directory `dir`.
    ///
    /// No path is resolved at all. The paths of the yielded entries are
    /// relative to `dir`, for instance `sub\file.txt`. The handle must have
    /// been opened with the `FILE_LIST_DIRECTORY` access right, for instance
    /// with [`std::fs::OpenOptions`] and `FILE_FLAG_BACKUP_SEMANTICS`.
    #[cfg(windows)]
    pub fn from_handle(dir: OwnedHandle) -> Self {
        Self::with_root(Root::Handle(Arc::new(dir)))
    }

    fn with_root(root: Root) -> Self {
        Self {
            entries: walk_dir(
                root.clone(),
                None::<Box<dyn FnMut(DirEntry) -> BoxedFut<Filtering> + Send>>,
            ),
            root,
        }
    }

    /// Filter entries.
    pub fn filter<F, Fut>(self, f: F) -> Self
    where
        F: FnMut(DirEntry) -> Fut + Send + 'static,
        Fut: Future<Output = Filtering> + Send,
    {
        Self {
            entries: walk_dir(self.root.clone(), Some(f)),
            root: self.root,
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

/// An entry yielded by [`WalkDir`].
///
/// The entry holds its parent directory handle, which stays open as long as
/// the entry (or one of its clones) is alive.
#[derive(Clone)]
pub struct DirEntry {
    parent: Arc<sys::Handle>,
    name: OsString,
    path: PathBuf,
    // File type found while listing the parent directory, when available.
    file_type: Option<FileType>,
    // Metadata found while listing the parent directory, when available.
    metadata: Option<Metadata>,
}

impl fmt::Debug for DirEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("DirEntry").field(&self.path).finish()
    }
}

impl DirEntry {
    /// Returns the path the entry had when its parent directory was listed.
    ///
    /// This path is informational. Opening or modifying the entry through it
    /// resolves it again and is subject to races: use [`DirEntry::open`] or
    /// the parent directory handle instead.
    pub fn path(&self) -> PathBuf {
        self.path.clone()
    }

    /// Returns the file name of the entry, relative to its parent directory.
    pub fn file_name(&self) -> OsString {
        self.name.clone()
    }

    /// Returns the file type of the entry, without following symbolic links.
    ///
    /// The file type is usually known from the directory listing and does not
    /// require any system call.
    pub async fn file_type(&self) -> io::Result<FileType> {
        match self.file_type {
            Some(file_type) => Ok(file_type),
            None => Ok(self.metadata().await?.file_type()),
        }
    }

    /// Returns the metadata of the entry, without following symbolic links:
    /// the metadata of a symbolic link describes the link itself.
    ///
    /// The metadata is read relative to the parent directory handle. It is a
    /// snapshot: to act on a file consistently with its metadata, use
    /// [`DirEntry::open`] and read the metadata of the opened file. On some
    /// platforms the metadata is the one found while listing the parent
    /// directory.
    pub async fn metadata(&self) -> io::Result<Metadata> {
        if let Some(metadata) = &self.metadata {
            return Ok(metadata.clone());
        }
        let (parent, name) = (self.parent.clone(), self.name.clone());
        unblock(move || sys::metadata(&parent, &name))
            .await
            .map(Metadata)
    }

    /// Opens the entry for reading, relative to its parent directory handle
    /// and without following symbolic links.
    ///
    /// Opening a symbolic link fails.
    pub async fn open(&self) -> io::Result<async_fs::File> {
        let (parent, name) = (self.parent.clone(), self.name.clone());
        unblock(move || sys::open_file(&parent, &name))
            .await
            .map(async_fs::File::from)
    }

    /// Returns the handle of the parent directory of this entry.
    ///
    /// Combined with [`DirEntry::file_name`], it allows acting on the entry
    /// with `*at` system calls (`openat`, `unlinkat`, `fstatat`, ...) without
    /// resolving its path again.
    #[cfg(unix)]
    pub fn parent_fd(&self) -> BorrowedFd<'_> {
        self.parent.as_fd()
    }

    /// Returns the handle of the parent directory of this entry.
    ///
    /// Combined with [`DirEntry::file_name`], it allows acting on the entry
    /// with `NtCreateFile` and a root directory handle, without resolving its
    /// path again. The handle is opened with the `FILE_LIST_DIRECTORY`,
    /// `FILE_READ_ATTRIBUTES` and `SYNCHRONIZE` access rights.
    #[cfg(windows)]
    pub fn parent_handle(&self) -> BorrowedHandle<'_> {
        self.parent.as_handle()
    }
}

/// The type of a [`DirEntry`], as returned by [`DirEntry::file_type`] and
/// [`Metadata::file_type`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FileType(sys::FileType);

impl FileType {
    /// Returns `true` if this is a directory.
    pub fn is_dir(&self) -> bool {
        self.0.is_dir()
    }

    /// Returns `true` if this is a regular file.
    pub fn is_file(&self) -> bool {
        self.0.is_file()
    }

    /// Returns `true` if this is a symbolic link.
    pub fn is_symlink(&self) -> bool {
        self.0.is_symlink()
    }
}

/// Metadata of a [`DirEntry`], as returned by [`DirEntry::metadata`].
///
/// Symbolic links are not followed: the metadata of a symbolic link
/// describes the link itself.
#[derive(Clone, Debug)]
pub struct Metadata(sys::Metadata);

impl Metadata {
    /// Returns the file type.
    pub fn file_type(&self) -> FileType {
        FileType(self.0.file_type())
    }

    /// Returns `true` if this is the metadata of a directory.
    pub fn is_dir(&self) -> bool {
        self.file_type().is_dir()
    }

    /// Returns `true` if this is the metadata of a regular file.
    pub fn is_file(&self) -> bool {
        self.file_type().is_file()
    }

    /// Returns `true` if this is the metadata of a symbolic link.
    pub fn is_symlink(&self) -> bool {
        self.file_type().is_symlink()
    }

    /// Returns the size in bytes.
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> u64 {
        self.0.len()
    }

    /// Returns `true` if the entry is read-only, with the same semantics as
    /// [`std::fs::Permissions::readonly`].
    pub fn readonly(&self) -> bool {
        self.0.readonly()
    }

    /// Returns the last modification time.
    pub fn modified(&self) -> io::Result<SystemTime> {
        self.0.modified()
    }

    /// Returns the last access time.
    pub fn accessed(&self) -> io::Result<SystemTime> {
        self.0.accessed()
    }

    /// Returns the creation time.
    ///
    /// Fails with [`io::ErrorKind::Unsupported`] on platforms or file
    /// systems that do not record it.
    pub fn created(&self) -> io::Result<SystemTime> {
        self.0.created()
    }
}

mod sealed {
    pub trait Sealed {}
    impl Sealed for super::FileType {}
    impl Sealed for super::Metadata {}
}

/// Unix specific extensions to [`FileType`].
///
/// This trait is sealed and cannot be implemented outside of this crate.
#[cfg(unix)]
pub trait FileTypeExt: sealed::Sealed {
    /// Returns `true` if this is a block device.
    fn is_block_device(&self) -> bool;
    /// Returns `true` if this is a character device.
    fn is_char_device(&self) -> bool;
    /// Returns `true` if this is a FIFO (named pipe).
    fn is_fifo(&self) -> bool;
    /// Returns `true` if this is a socket.
    fn is_socket(&self) -> bool;
}

#[cfg(unix)]
impl FileTypeExt for FileType {
    fn is_block_device(&self) -> bool {
        self.0.is_block_device()
    }
    fn is_char_device(&self) -> bool {
        self.0.is_char_device()
    }
    fn is_fifo(&self) -> bool {
        self.0.is_fifo()
    }
    fn is_socket(&self) -> bool {
        self.0.is_socket()
    }
}

/// Unix specific extensions to [`Metadata`].
///
/// This trait is sealed and cannot be implemented outside of this crate.
#[cfg(unix)]
pub trait MetadataExt: sealed::Sealed {
    /// Returns the ID of the device containing the file.
    fn dev(&self) -> u64;
    /// Returns the inode number.
    fn ino(&self) -> u64;
    /// Returns the file type and mode bits (`st_mode`).
    fn mode(&self) -> u32;
    /// Returns the number of hard links.
    fn nlink(&self) -> u64;
    /// Returns the user ID of the owner.
    fn uid(&self) -> u32;
    /// Returns the group ID of the owner.
    fn gid(&self) -> u32;
}

#[cfg(unix)]
impl MetadataExt for Metadata {
    fn dev(&self) -> u64 {
        self.0.dev
    }
    fn ino(&self) -> u64 {
        self.0.ino
    }
    fn mode(&self) -> u32 {
        self.0.mode
    }
    fn nlink(&self) -> u64 {
        self.0.nlink
    }
    fn uid(&self) -> u32 {
        self.0.uid
    }
    fn gid(&self) -> u32 {
        self.0.gid
    }
}

/// Windows specific extensions to [`Metadata`].
///
/// This trait is sealed and cannot be implemented outside of this crate.
#[cfg(windows)]
pub trait MetadataExt: sealed::Sealed {
    /// Returns the file attributes (`FILE_ATTRIBUTE_*` flags).
    fn file_attributes(&self) -> u32;
    /// Returns the file index (file ID) identifying the file on its volume.
    fn file_index(&self) -> u64;
}

#[cfg(windows)]
impl MetadataExt for Metadata {
    fn file_attributes(&self) -> u32 {
        self.0.attributes
    }
    fn file_index(&self) -> u64 {
        self.0.file_index
    }
}

fn walk_dir<F, Fut>(root: Root, filter: Option<F>) -> BoxStream
where
    F: FnMut(DirEntry) -> Fut + Send + 'static,
    Fut: Future<Output = Filtering> + Send,
{
    stream::unfold(State::Start((root, filter)), move |state| async move {
        match state {
            State::Start((root, filter)) => {
                let (path, opened) = match root {
                    Root::Path(path) => {
                        let p = path.clone();
                        let opened = unblock(move || {
                            let dir = sys::open_root(&p)?;
                            let lister = sys::Lister::new(&dir)?;
                            Ok((Arc::new(dir), lister))
                        })
                        .await;
                        (path, opened)
                    }
                    Root::Handle(dir) => {
                        let opened = unblock(move || {
                            let lister = sys::Lister::new(&dir)?;
                            Ok((dir, lister))
                        })
                        .await;
                        (PathBuf::new(), opened)
                    }
                };
                match opened {
                    Err(source) => Some((Err(InnerError::Io { path, source }.into()), State::Done)),
                    Ok((dir, lister)) => walk(vec![Frame::new(path, dir, lister)], filter).await,
                }
            }
            State::Walk((frames, filter)) => walk(frames, filter).await,
            State::Done => None,
        }
    })
    .boxed()
}

enum State<F> {
    Start((Root, Option<F>)),
    Walk((Vec<Frame>, Option<F>)),
    Done,
}

type UnfoldState<F> = (Result<DirEntry>, State<F>);

/// A directory being walked.
struct Frame {
    path: PathBuf,
    dir: Arc<sys::Handle>,
    // `None` once the directory has been fully listed.
    lister: Option<sys::Lister>,
    // Entries listed but not processed yet.
    pending: VecDeque<io::Result<sys::RawEntry>>,
}

impl Frame {
    fn new(path: PathBuf, dir: Arc<sys::Handle>, lister: sys::Lister) -> Self {
        Self {
            path,
            dir,
            lister: Some(lister),
            pending: VecDeque::new(),
        }
    }
}

// Iterative on purpose: filtered-out entries must not grow the stack (see issue #13).
async fn walk<F, Fut>(mut frames: Vec<Frame>, mut filter: Option<F>) -> Option<UnfoldState<F>>
where
    F: FnMut(DirEntry) -> Fut + Send + 'static,
    Fut: Future<Output = Filtering> + Send,
{
    loop {
        let frame = frames.last_mut()?;
        let raw = match frame.pending.pop_front() {
            Some(raw) => raw,
            None => {
                let Some(mut lister) = frame.lister.take() else {
                    frames.pop();
                    continue;
                };
                let (lister, batch) = unblock(move || {
                    let batch = lister.next_batch(BATCH_SIZE);
                    (lister, batch)
                })
                .await;
                // A short batch means the listing is over (end of directory or
                // error): dropping the lister releases its file descriptor early.
                if batch.len() == BATCH_SIZE {
                    frame.lister = Some(lister);
                }
                frame.pending = batch.into();
                continue;
            }
        };
        let raw = match raw {
            Ok(raw) => raw,
            Err(source) => {
                let path = frame.path.clone();
                return io_error(path, source, frames, filter);
            }
        };
        let entry = DirEntry {
            parent: frame.dir.clone(),
            path: frame.path.join(&raw.name),
            name: raw.name,
            file_type: raw.file_type.map(FileType),
            metadata: raw.metadata.map(Metadata),
        };
        let ft = match entry.file_type().await {
            Ok(ft) => ft,
            Err(source) => return io_error(entry.path, source, frames, filter),
        };
        let filtering = match filter.as_mut() {
            Some(filter) => filter(entry.clone()).await,
            None => Filtering::Continue,
        };
        if ft.is_dir() && filtering != Filtering::IgnoreDir {
            // The file type above comes from the listing and may be stale by now:
            // `sys::open_dir` is the actual check, it refuses anything that is not a
            // directory at the time of the open, symbolic links included.
            let (parent, name) = (entry.parent.clone(), entry.name.clone());
            let opened = unblock(move || {
                let dir = sys::open_dir(&parent, &name)?;
                let lister = sys::Lister::new(&dir)?;
                Ok((Arc::new(dir), lister))
            })
            .await;
            match opened {
                Ok((dir, lister)) => frames.push(Frame::new(entry.path.clone(), dir, lister)),
                Err(source) => return io_error(entry.path, source, frames, filter),
            }
        }
        if filtering == Filtering::Continue {
            return Some((Ok(entry), State::Walk((frames, filter))));
        }
    }
}

fn io_error<F>(
    path: PathBuf,
    source: io::Error,
    frames: Vec<Frame>,
    filter: Option<F>,
) -> Option<UnfoldState<F>> {
    let err = InnerError::Io { path, source }.into();
    Some((Err(err), State::Walk((frames, filter))))
}
