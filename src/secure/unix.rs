//! Unix backend of the secure walker, built on `openat(2)` and friends.
//!
//! Every access is relative to an already opened directory file descriptor
//! and names a single path component read from that directory (`readdir(3)`
//! never returns names containing `/`). Combined with `O_NOFOLLOW` and
//! `AT_SYMLINK_NOFOLLOW`, the kernel resolves exactly one component per call
//! and never follows a symbolic link on our behalf.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rustix::fs::{self as rfs, AtFlags, Mode, OFlags};

/// An open directory.
pub(super) type Handle = OwnedFd;

/// Opens the root directory, following symbolic links: callers must trust it.
pub(super) fn open_root(path: &Path) -> io::Result<Handle> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    Ok(rfs::open(path, flags, Mode::empty())?)
}

/// Opens the directory `name` of `parent`, refusing anything else.
///
/// This is the security check of the walker: the file type seen while
/// listing `parent` is only a hint, which a concurrent process may have
/// invalidated since.
/// - `O_NOFOLLOW` makes the call fail if `name` is a symbolic link (`ELOOP`,
///   `EMLINK` on FreeBSD). It only applies to the last path component, which
///   is the only one here.
/// - `O_DIRECTORY` makes it fail with `ENOTDIR` for anything that is not a
///   directory. The check happens before opening, so a FIFO swapped in
///   cannot block the call.
pub(super) fn open_dir(parent: &Handle, name: &OsStr) -> io::Result<Handle> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    Ok(rfs::openat(parent, name, flags, Mode::empty())?)
}

/// Opens the entry `name` of `parent` for reading, refusing symbolic links.
pub(super) fn open_file(parent: &Handle, name: &OsStr) -> io::Result<File> {
    // - `O_NONBLOCK`: opening a FIFO for reading blocks until a writer shows
    //   up, which could hang a thread of the blocking pool forever. It is
    //   cleared right after the open so that the returned file behaves like
    //   any `std::fs::File`.
    // - `O_NOCTTY`: never let a terminal become the controlling terminal of
    //   the process.
    let flags =
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK | OFlags::NOCTTY;
    let fd = rfs::openat(parent, name, flags, Mode::empty())?;
    let flags = rfs::fcntl_getfl(&fd)?;
    rfs::fcntl_setfl(&fd, flags - OFlags::NONBLOCK)?;
    Ok(File::from(fd))
}

/// An entry read from a directory listing.
pub(super) struct RawEntry {
    pub(super) name: OsString,
    pub(super) file_type: Option<FileType>,
    // Unix listings do not provide metadata.
    pub(super) metadata: Option<Metadata>,
}

/// Lists the entries of an open directory.
pub(super) struct Lister(rfs::Dir);

impl Lister {
    pub(super) fn new(dir: &Handle) -> io::Result<Self> {
        // `Dir::read_from` does not `fdopendir` the given descriptor but a new
        // one, obtained by opening "." relative to it. The listing then has
        // its own read position, which callers holding the directory handle
        // (see `DirEntry::parent_fd`) cannot disturb.
        Ok(Self(rfs::Dir::read_from(dir)?))
    }

    /// Reads up to `max` entries. Returns less than `max` entries only once
    /// the listing is over, either at the end of the directory or after an
    /// error, which is then the last element.
    pub(super) fn next_batch(&mut self, max: usize) -> Vec<io::Result<RawEntry>> {
        let mut batch = Vec::with_capacity(max);
        while batch.len() < max {
            match self.0.read() {
                None => break,
                Some(Err(err)) => {
                    // `Dir` stops listing after an error.
                    batch.push(Err(err.into()));
                    break;
                }
                Some(Ok(entry)) => {
                    let name = entry.file_name().to_bytes();
                    if name == b"." || name == b".." {
                        continue;
                    }
                    // `DT_UNKNOWN` is returned by file systems that do not store
                    // the file type in directories: fall back to `fstatat`.
                    let file_type = match entry.file_type() {
                        rfs::FileType::Unknown => None,
                        file_type => Some(FileType(file_type)),
                    };
                    batch.push(Ok(RawEntry {
                        name: OsStr::from_bytes(name).to_owned(),
                        file_type,
                        metadata: None,
                    }));
                }
            }
        }
        batch
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct FileType(rfs::FileType);

// `rustix::fs::FileType` does not implement `Hash`: hash its discriminant,
// which is consistent with its derived `PartialEq`.
impl std::hash::Hash for FileType {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::mem::discriminant(&self.0).hash(state);
    }
}

impl FileType {
    pub(super) fn is_dir(&self) -> bool {
        self.0 == rfs::FileType::Directory
    }
    pub(super) fn is_file(&self) -> bool {
        self.0 == rfs::FileType::RegularFile
    }
    pub(super) fn is_symlink(&self) -> bool {
        self.0 == rfs::FileType::Symlink
    }
    pub(super) fn is_block_device(&self) -> bool {
        self.0 == rfs::FileType::BlockDevice
    }
    pub(super) fn is_char_device(&self) -> bool {
        self.0 == rfs::FileType::CharacterDevice
    }
    pub(super) fn is_fifo(&self) -> bool {
        self.0 == rfs::FileType::Fifo
    }
    pub(super) fn is_socket(&self) -> bool {
        self.0 == rfs::FileType::Socket
    }
}

#[derive(Clone, Debug)]
pub(super) struct Metadata {
    file_type: FileType,
    pub(super) mode: u32,
    pub(super) dev: u64,
    pub(super) ino: u64,
    pub(super) nlink: u64,
    pub(super) uid: u32,
    pub(super) gid: u32,
    size: u64,
    accessed: Timestamp,
    modified: Timestamp,
    created: Option<Timestamp>,
}

impl Metadata {
    pub(super) fn file_type(&self) -> FileType {
        self.file_type
    }
    pub(super) fn len(&self) -> u64 {
        self.size
    }
    pub(super) fn readonly(&self) -> bool {
        // Same definition as `std::fs::Permissions::readonly` on Unix.
        self.mode & 0o222 == 0
    }
    pub(super) fn modified(&self) -> io::Result<SystemTime> {
        self.modified.to_system_time()
    }
    pub(super) fn accessed(&self) -> io::Result<SystemTime> {
        self.accessed.to_system_time()
    }
    pub(super) fn created(&self) -> io::Result<SystemTime> {
        match self.created {
            Some(created) => created.to_system_time(),
            None => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "creation time is not available on this platform or file system",
            )),
        }
    }

    // The field types of `struct stat` vary across platforms: the casts are
    // needed on some of them and redundant on others.
    #[allow(clippy::unnecessary_cast)]
    fn from_stat(st: &rfs::Stat) -> Self {
        Self {
            file_type: FileType(rfs::FileType::from_raw_mode(st.st_mode as _)),
            mode: st.st_mode as u32,
            dev: st.st_dev as u64,
            ino: st.st_ino as u64,
            nlink: st.st_nlink as u64,
            uid: st.st_uid as u32,
            gid: st.st_gid as u32,
            size: st.st_size as u64,
            accessed: Timestamp::new(st.st_atime as i64, st.st_atime_nsec as i64),
            modified: Timestamp::new(st.st_mtime as i64, st.st_mtime_nsec as i64),
            created: birth_time(st),
        }
    }
}

/// Reads the metadata of the entry `name` of `parent`, without following
/// symbolic links.
pub(super) fn metadata(parent: &Handle, name: &OsStr) -> io::Result<Metadata> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    match statx_metadata(parent, name) {
        Ok(metadata) => return Ok(metadata),
        // `statx` is missing on Linux < 4.11, and some seccomp filters (older
        // container runtimes) reject it with `EPERM`: fall back to `fstatat`,
        // without the creation time. `std` applies the same fallback.
        Err(rustix::io::Errno::NOSYS) | Err(rustix::io::Errno::PERM) => {}
        Err(err) => return Err(err.into()),
    }
    let st = rfs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)?;
    Ok(Metadata::from_stat(&st))
}

/// Linux only records the creation time in `statx`.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn statx_metadata(parent: &Handle, name: &OsStr) -> rustix::io::Result<Metadata> {
    use rfs::StatxFlags;

    let stx = rfs::statx(
        parent,
        name,
        AtFlags::SYMLINK_NOFOLLOW,
        StatxFlags::BASIC_STATS | StatxFlags::BTIME,
    )?;
    let ts = |t: rfs::StatxTimestamp| Timestamp::new(t.tv_sec, i64::from(t.tv_nsec));
    // The file system may not support the creation time even if asked for.
    let created = StatxFlags::from_bits_retain(stx.stx_mask)
        .contains(StatxFlags::BTIME)
        .then(|| ts(stx.stx_btime));
    Ok(Metadata {
        file_type: FileType(rfs::FileType::from_raw_mode(stx.stx_mode.into())),
        mode: stx.stx_mode.into(),
        dev: rfs::makedev(stx.stx_dev_major, stx.stx_dev_minor),
        ino: stx.stx_ino,
        nlink: stx.stx_nlink.into(),
        uid: stx.stx_uid,
        gid: stx.stx_gid,
        size: stx.stx_size,
        accessed: ts(stx.stx_atime),
        modified: ts(stx.stx_mtime),
        created,
    })
}

#[cfg(any(target_vendor = "apple", target_os = "freebsd"))]
#[allow(clippy::unnecessary_cast)] // See `Metadata::from_stat`.
fn birth_time(st: &rfs::Stat) -> Option<Timestamp> {
    Some(Timestamp::new(
        st.st_birthtime as i64,
        st.st_birthtime_nsec as i64,
    ))
}

#[cfg(not(any(target_vendor = "apple", target_os = "freebsd")))]
fn birth_time(_: &rfs::Stat) -> Option<Timestamp> {
    None
}

/// A time as seconds and nanoseconds since the Unix epoch.
#[derive(Clone, Copy, Debug)]
struct Timestamp {
    secs: i64,
    nanos: u32,
}

impl Timestamp {
    fn new(secs: i64, nanos: i64) -> Self {
        // Guard against out of range values from exotic file systems rather
        // than letting `Duration::new` carry them into the seconds.
        let nanos = u32::try_from(nanos)
            .ok()
            .filter(|n| *n < 1_000_000_000)
            .unwrap_or(0);
        Self { secs, nanos }
    }

    fn to_system_time(self) -> io::Result<SystemTime> {
        // Checked arithmetic: `SystemTime` cannot represent every `i64` of
        // seconds, and the unchecked operators would panic.
        let nanos = Duration::from_nanos(self.nanos.into());
        let time = if self.secs >= 0 {
            UNIX_EPOCH.checked_add(Duration::from_secs(self.secs.unsigned_abs()))
        } else {
            UNIX_EPOCH.checked_sub(Duration::from_secs(self.secs.unsigned_abs()))
        };
        time.and_then(|t| t.checked_add(nanos))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "timestamp out of range"))
    }
}
