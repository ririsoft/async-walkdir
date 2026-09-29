//! Windows backend of the secure walker, built on `NtCreateFile` and
//! `GetFileInformationByHandleEx`.
//!
//! Every access is relative to an already opened directory handle
//! (`OBJECT_ATTRIBUTES::RootDirectory`) and names a single path component
//! read from that directory. `FILE_OPEN_REPARSE_POINT` opens symbolic links
//! and junctions themselves instead of their target, and the opened handle is
//! then checked to refuse them. This is the approach used by
//! `std::fs::remove_dir_all` since the fix of CVE-2022-21658.
//!
//! This is the only module of the crate allowed to use `unsafe`, to call
//! Windows APIs that `std` does not expose. Every `unsafe` block documents
//! why it is sound. Parsing of the directory listing is done with safe,
//! bounds-checked code.

#![allow(unsafe_code)]

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io;
use std::mem::{offset_of, size_of};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::ptr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    NtCreateFile, FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN,
    FILE_OPEN_FOR_BACKUP_INTENT, FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT,
    NTCREATEFILE_CREATE_OPTIONS,
};
use windows_sys::Win32::Foundation::{
    RtlNtStatusToDosError, ERROR_DIRECTORY, ERROR_NO_MORE_FILES, ERROR_STOPPED_ON_SYMLINK, HANDLE,
    NTSTATUS, UNICODE_STRING,
};
use windows_sys::Win32::Storage::FileSystem::{
    FileAttributeTagInfo, FileIdBothDirectoryInfo, GetFileInformationByHandleEx,
    FILE_ACCESS_RIGHTS, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_READONLY,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_GENERIC_READ, FILE_ID_BOTH_DIR_INFO, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, SYNCHRONIZE,
};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

/// An open directory.
pub(super) type Handle = OwnedHandle;

/// Size of the buffer receiving directory listings. 64 KiB is the maximum
/// supported by SMB network shares.
const LISTING_BUFFER_SIZE: usize = 64 * 1024;

/// Opens the root directory, following symbolic links: callers must trust it.
pub(super) fn open_root(path: &Path) -> io::Result<Handle> {
    // `FILE_FLAG_BACKUP_SEMANTICS` is required to open a directory.
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    if !file.metadata()?.is_dir() {
        return Err(io::Error::from_raw_os_error(ERROR_DIRECTORY as i32));
    }
    Ok(file.into())
}

/// Opens the directory `name` of `parent`, refusing anything else.
///
/// This is the security check of the walker: the file type seen while
/// listing `parent` is only a hint, which a concurrent process may have
/// invalidated since. `FILE_DIRECTORY_FILE` refuses anything that is not a
/// directory, and `check_not_link` refuses symbolic links and junctions.
pub(super) fn open_dir(parent: &Handle, name: &OsStr) -> io::Result<Handle> {
    // `FILE_OPEN_FOR_BACKUP_INTENT` is what `FILE_FLAG_BACKUP_SEMANTICS` maps
    // to, so that directories are opened like `std` and `open_root` do.
    let dir = nt_open(
        parent,
        name,
        FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES,
        FILE_DIRECTORY_FILE | FILE_OPEN_FOR_BACKUP_INTENT,
    )?;
    check_not_link(&dir)?;
    Ok(dir)
}

/// Opens the entry `name` of `parent` for reading, refusing symbolic links
/// and junctions.
pub(super) fn open_file(parent: &Handle, name: &OsStr) -> io::Result<File> {
    let file = nt_open(parent, name, FILE_GENERIC_READ, FILE_NON_DIRECTORY_FILE)?;
    check_not_link(&file)?;
    Ok(file.into())
}

/// Windows listings always provide the metadata (see `Lister`), so this
/// fallback, required by the platform independent code, is never reached.
pub(super) fn metadata(_parent: &Handle, _name: &OsStr) -> io::Result<Metadata> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "metadata is only available from directory listings on Windows",
    ))
}

/// Opens `name` relative to the directory `parent`, without following
/// symbolic links or junctions.
///
/// An empty `name` opens `parent` itself again, with a new file object.
fn nt_open(
    parent: &Handle,
    name: &OsStr,
    access: FILE_ACCESS_RIGHTS,
    options: NTCREATEFILE_CREATE_OPTIONS,
) -> io::Result<Handle> {
    let wide: Vec<u16> = name.encode_wide().collect();
    // `UNICODE_STRING` lengths are in bytes, and limited to `u16`.
    let len = wide
        .len()
        .checked_mul(2)
        .and_then(|len| u16::try_from(len).ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "file name too long"))?;
    let object_name = UNICODE_STRING {
        Length: len,
        MaximumLength: len,
        // Never written through: `NtCreateFile` takes the name as input only.
        Buffer: wide.as_ptr().cast_mut(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: parent.as_raw_handle(),
        ObjectName: &object_name,
        // No `OBJ_CASE_INSENSITIVE`: `name` is the exact name returned by the
        // listing, and a case insensitive lookup could open another entry in
        // case sensitive directories.
        Attributes: 0,
        SecurityDescriptor: ptr::null(),
        SecurityQualityOfService: ptr::null(),
    };
    let mut handle: HANDLE = ptr::null_mut();
    let mut io_status = IO_STATUS_BLOCK::default();
    // SAFETY:
    // - `handle` and `io_status` are valid for writes for the whole call.
    // - `attributes` is fully initialized. It points to `object_name`, which
    //   points to `wide`: all of them outlive the call, and `Length` is the
    //   size in bytes of `wide` (with a zero length, `Buffer` is never read).
    // - `RootDirectory` is a valid directory handle, borrowed from `parent`
    //   for the duration of the call.
    // - The allocation size and extended attributes are optional: null
    //   pointers with a zero length.
    // - `FILE_SYNCHRONOUS_IO_NONALERT` makes the handle synchronous, as
    //   expected by `std::fs::File`; it requires the `SYNCHRONIZE` right.
    let status: NTSTATUS = unsafe {
        NtCreateFile(
            &mut handle,
            access | SYNCHRONIZE,
            &attributes,
            &mut io_status,
            ptr::null(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            FILE_OPEN,
            options | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
            ptr::null(),
            0,
        )
    };
    // Negative `NTSTATUS` values are errors (`NT_SUCCESS` macro).
    if status < 0 {
        // SAFETY: `RtlNtStatusToDosError` only converts an integer.
        let code = unsafe { RtlNtStatusToDosError(status) };
        return Err(io::Error::from_raw_os_error(code as i32));
    }
    // SAFETY: `NtCreateFile` succeeded, so `handle` is a valid handle that
    // nothing else owns: `OwnedHandle` takes care of closing it.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

/// Fails if `handle` is a symbolic link or a junction.
///
/// `FILE_OPEN_REPARSE_POINT` does not refuse to open links: it opens the
/// link itself instead of its target. Descending into it would not escape
/// the walked tree, but the swap must be reported, not silently ignored.
fn check_not_link(handle: &Handle) -> io::Result<()> {
    let mut info = FILE_ATTRIBUTE_TAG_INFO {
        FileAttributes: 0,
        ReparseTag: 0,
    };
    // SAFETY: `handle` is a valid handle opened with `FILE_READ_ATTRIBUTES`
    // (included in `FILE_GENERIC_READ`), and `info` is a properly aligned
    // `FILE_ATTRIBUTE_TAG_INFO`, valid for writes of the given size, as
    // expected for the `FileAttributeTagInfo` class.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            handle.as_raw_handle(),
            FileAttributeTagInfo,
            (&mut info as *mut FILE_ATTRIBUTE_TAG_INFO).cast(),
            size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    if is_link(info.FileAttributes, info.ReparseTag) {
        return Err(io::Error::from_raw_os_error(
            ERROR_STOPPED_ON_SYMLINK as i32,
        ));
    }
    Ok(())
}

/// Returns `true` for symbolic links and junctions, with the same
/// definition as `std`: reparse points whose tag is a "name surrogate", i.e.
/// that redirect to another file system location. Other reparse points
/// (deduplicated or cloud files, ...) are regular files and directories.
fn is_link(attributes: u32, reparse_tag: u32) -> bool {
    // `IsReparseTagNameSurrogate` macro of `winnt.h`.
    const NAME_SURROGATE: u32 = 0x2000_0000;
    attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 && reparse_tag & NAME_SURROGATE != 0
}

/// An entry read from a directory listing.
pub(super) struct RawEntry {
    pub(super) name: OsString,
    pub(super) file_type: Option<FileType>,
    pub(super) metadata: Option<Metadata>,
}

/// Lists the entries of an open directory.
pub(super) struct Lister {
    handle: Handle,
    // `u64` elements: `GetFileInformationByHandleEx` requires an 8 bytes
    // aligned buffer for `FILE_ID_BOTH_DIR_INFO` records.
    buffer: Vec<u64>,
    pending: VecDeque<io::Result<RawEntry>>,
    done: bool,
}

impl Lister {
    pub(super) fn new(dir: &Handle) -> io::Result<Self> {
        // The listing position is attached to the file object, which handles
        // share when duplicated. Opening the directory again gives the
        // listing its own position, which callers holding the directory
        // handle (see `DirEntry::parent_handle`) cannot disturb.
        let handle = nt_open(
            dir,
            OsStr::new(""),
            FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES,
            FILE_DIRECTORY_FILE | FILE_OPEN_FOR_BACKUP_INTENT,
        )?;
        Ok(Self {
            handle,
            buffer: vec![0; LISTING_BUFFER_SIZE / size_of::<u64>()],
            pending: VecDeque::new(),
            done: false,
        })
    }

    /// Reads up to `max` entries. Returns less than `max` entries only once
    /// the listing is over, either at the end of the directory or after an
    /// error, which is then the last element.
    pub(super) fn next_batch(&mut self, max: usize) -> Vec<io::Result<RawEntry>> {
        while self.pending.len() < max && !self.done {
            self.fill();
        }
        let len = self.pending.len().min(max);
        self.pending.drain(..len).collect()
    }

    /// Reads the next chunk of the listing into `self.pending`.
    fn fill(&mut self) {
        let size = self.buffer.len() * size_of::<u64>();
        // SAFETY: `self.handle` is a valid directory handle opened with
        // `FILE_LIST_DIRECTORY`, and `self.buffer` is valid for writes of
        // `size` bytes and 8 bytes aligned, as `FileIdBothDirectoryInfo`
        // requires. Successive calls continue the listing.
        let ok = unsafe {
            GetFileInformationByHandleEx(
                self.handle.as_raw_handle(),
                FileIdBothDirectoryInfo,
                self.buffer.as_mut_ptr().cast(),
                size as u32,
            )
        };
        if ok == 0 {
            self.done = true;
            let err = io::Error::last_os_error();
            if err.raw_os_error() != Some(ERROR_NO_MORE_FILES as i32) {
                self.pending.push_back(Err(err));
            }
            return;
        }
        // SAFETY: the buffer is a live, initialized allocation of `size`
        // bytes, and `u8` has no alignment or validity requirements. The
        // byte view does not outlive this function and the buffer is not
        // modified meanwhile.
        let bytes = unsafe { std::slice::from_raw_parts(self.buffer.as_ptr().cast::<u8>(), size) };
        if let Err(err) = parse_listing(bytes, &mut self.pending) {
            self.done = true;
            self.pending.push_back(Err(err));
        }
    }
}

/// Parses a chain of `FILE_ID_BOTH_DIR_INFO` records.
///
/// Fields are read with bounds-checked accessors instead of casting the
/// buffer to the structure, so that a malformed listing can only produce an
/// error, never an out of bounds read.
fn parse_listing(bytes: &[u8], out: &mut VecDeque<io::Result<RawEntry>>) -> io::Result<()> {
    type Info = FILE_ID_BOTH_DIR_INFO;
    let malformed = || io::Error::new(io::ErrorKind::InvalidData, "malformed directory listing");

    let mut offset = 0;
    loop {
        let record = bytes.get(offset..).ok_or_else(malformed)?;
        let field_u32 = |at: usize| read_u32(record, at).ok_or_else(malformed);
        let field_i64 = |at: usize| read_i64(record, at).ok_or_else(malformed);

        let name_len = field_u32(offset_of!(Info, FileNameLength))? as usize;
        let name_start = offset_of!(Info, FileName);
        let name_end = name_start.checked_add(name_len).ok_or_else(malformed)?;
        let name = record.get(name_start..name_end).ok_or_else(malformed)?;
        let name: Vec<u16> = name
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_ne_bytes(*c))
            .collect();

        if name != [u16::from(b'.')] && name != [u16::from(b'.'), u16::from(b'.')] {
            let attributes = field_u32(offset_of!(Info, FileAttributes))?;
            // For reparse points, `EaSize` holds the reparse tag instead of
            // the extended attributes size ([MS-FSCC] 2.4.17).
            let reparse_tag = if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                field_u32(offset_of!(Info, EaSize))?
            } else {
                0
            };
            let file_type = FileType::new(attributes, reparse_tag);
            let metadata = Metadata {
                file_type,
                attributes,
                size: field_i64(offset_of!(Info, EndOfFile))? as u64,
                created: field_i64(offset_of!(Info, CreationTime))?,
                accessed: field_i64(offset_of!(Info, LastAccessTime))?,
                modified: field_i64(offset_of!(Info, LastWriteTime))?,
                file_index: field_i64(offset_of!(Info, FileId))? as u64,
            };
            out.push_back(Ok(RawEntry {
                name: OsString::from_wide(&name),
                file_type: Some(file_type),
                metadata: Some(metadata),
            }));
        }

        match field_u32(offset_of!(Info, NextEntryOffset))? {
            0 => return Ok(()),
            next => offset = offset.checked_add(next as usize).ok_or_else(malformed)?,
        }
    }
}

fn read_u32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_ne_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

fn read_i64(bytes: &[u8], at: usize) -> Option<i64> {
    Some(i64::from_ne_bytes(bytes.get(at..at + 8)?.try_into().ok()?))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct FileType {
    is_directory: bool,
    is_link: bool,
}

impl FileType {
    fn new(attributes: u32, reparse_tag: u32) -> Self {
        Self {
            is_directory: attributes & FILE_ATTRIBUTE_DIRECTORY != 0,
            is_link: is_link(attributes, reparse_tag),
        }
    }
    // Same semantics as `std` on Windows: a link to a directory is a symbolic
    // link, neither a directory nor a file.
    pub(super) fn is_dir(&self) -> bool {
        !self.is_link && self.is_directory
    }
    pub(super) fn is_file(&self) -> bool {
        !self.is_link && !self.is_directory
    }
    pub(super) fn is_symlink(&self) -> bool {
        self.is_link
    }
}

#[derive(Clone, Debug)]
pub(super) struct Metadata {
    file_type: FileType,
    pub(super) attributes: u32,
    pub(super) file_index: u64,
    size: u64,
    // `FILETIME`s: 100 ns intervals since 1601-01-01 UTC.
    created: i64,
    accessed: i64,
    modified: i64,
}

impl Metadata {
    pub(super) fn file_type(&self) -> FileType {
        self.file_type
    }
    pub(super) fn len(&self) -> u64 {
        self.size
    }
    pub(super) fn readonly(&self) -> bool {
        // Same definition as `std::fs::Permissions::readonly` on Windows.
        self.attributes & FILE_ATTRIBUTE_READONLY != 0
    }
    pub(super) fn modified(&self) -> io::Result<SystemTime> {
        filetime_to_system_time(self.modified)
    }
    pub(super) fn accessed(&self) -> io::Result<SystemTime> {
        filetime_to_system_time(self.accessed)
    }
    pub(super) fn created(&self) -> io::Result<SystemTime> {
        filetime_to_system_time(self.created)
    }
}

fn filetime_to_system_time(filetime: i64) -> io::Result<SystemTime> {
    const INTERVALS_PER_SEC: u64 = 10_000_000;
    // Number of 100 ns intervals between 1601-01-01 and 1970-01-01.
    const UNIX_EPOCH_AS_FILETIME: i64 = 116_444_736_000_000_000;

    let out_of_range = || io::Error::new(io::ErrorKind::InvalidData, "timestamp out of range");
    let since_unix_epoch = filetime
        .checked_sub(UNIX_EPOCH_AS_FILETIME)
        .ok_or_else(out_of_range)?;
    let abs = since_unix_epoch.unsigned_abs();
    // Split in seconds and nanoseconds: converting all the intervals to
    // nanoseconds at once could overflow `u64`.
    let duration = Duration::new(
        abs / INTERVALS_PER_SEC,
        ((abs % INTERVALS_PER_SEC) * 100) as u32,
    );
    let time = if since_unix_epoch >= 0 {
        UNIX_EPOCH.checked_add(duration)
    } else {
        UNIX_EPOCH.checked_sub(duration)
    };
    time.ok_or_else(out_of_range)
}
