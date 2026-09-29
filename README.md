[![Github CI](https://github.com/ririsoft/async-walkdir/workflows/Rust/badge.svg)](https://github.com/ririsoft/async-walkdir/actions) [![docs.rs](https://docs.rs/async-walkdir/badge.svg)](https://docs.rs/async-walkdir)
# async-walkdir
Asynchronous directory traversal for Rust.

Based on [async-fs][2] and [blocking][3],
it uses a thread pool to handle blocking IOs. Please refere to those crates for the rationale.
This crate is compatible with async runtimes [tokio][5], [async-std][6], [smol][7] and potentially any runtime based on [futures 0.3][4]

We do not plan to be as feature full as [Walkdir][1] crate in the synchronous world, but
do not hesitate to open an issue or a PR.

# Example

```rust
use async_walkdir::WalkDir;
use futures_lite::future::block_on;
use futures_lite::stream::StreamExt;

block_on(async {
    let mut entries = WalkDir::new("my_directory");
    loop {
        match entries.next().await {
            Some(Ok(entry)) => println!("file: {}", entry.path().display()),
            Some(Err(e)) => {
                eprintln!("error: {}", e);
                break;
            },
            None => break,
        }
    }
});
```

# Minimum supported Rust version

The minimum supported Rust version (MSRV) is **1.85**, with every Cargo feature and on
every supported platform. It is checked by the CI.

The MSRV may be raised in a minor release, never in a patch release. Rust versions released
during the last 12 months remain supported.

# Security: symlinks and concurrent modifications

`WalkDir` does not follow symbolic links: a symlink is yielded as an entry but the walker
never descends into it. This guarantee only holds **while nobody else modifies the
directory tree during the walk**.

## The race

For each entry, the walker first checks whether it is a directory without following
symlinks, then opens it again *by path* to list its content. Opening by path resolves every
component of the path again and follows symlinks. Between the check and the open, a
concurrent process can replace a directory with a symlink:

```text
root/
└── uploads/          1. walker checks `root/uploads`: a real directory, ok
                      2. attacker: mv root/uploads root/x && ln -s /etc root/uploads
                      3. walker opens `root/uploads` → actually lists /etc
```

The walker then yields entries located outside of `root`, while their paths still look
like they are inside it (`root/uploads/passwd`). A check such as
`entry.path().starts_with(root)` does not detect it. The swap can be repeated deeper in
the tree, and replacing any *parent* directory has the same effect.

The unit test `known_limitation_swapped_dir_is_followed` in [`src/lib.rs`](src/lib.rs)
reproduces this attack deterministically on Unix and Windows. It swaps a directory for a
symlink (a junction on Windows) at the right time and checks that the walk escapes the
root. Run it with `cargo test known_limitation`. The `secure` walker described below
passes the same scenario without escaping: run
`cargo test --features secure swapped_dir_is_not_followed`.

This is a classic time-of-check to time-of-use (TOCTOU) race. It is the same class of issue
as [CVE-2022-21658][8] in `std::fs::remove_dir_all`, and it also affects the synchronous
[walkdir][1] crate ([BurntSushi/walkdir#209][9]).

## Am I affected?

You are affected only if **both** conditions are true:

- the walked tree can be modified by someone you do not trust (shared or world-writable
  directories such as `/tmp`, upload folders, other users' home directories, container
  volumes, ...), **and**
- your process has privileges that this someone does not have (runs as root, as a service
  account, with access to secrets, ...), and acts on the entries: deleting, copying,
  changing ownership or permissions, serving or indexing them.

Walking your own files, a build directory, or any tree only writable by the user running
the walk is **not** affected.

## Mitigations

- Do not walk directories writable by less privileged users from a privileged process.
  Drop privileges to the owner of the tree before walking it when possible.
- Remember that acting on `entry.path()` after the walk is racy as well: the path is
  resolved again by every subsequent file system call. Re-validate with
  `std::fs::symlink_metadata` right before acting and treat any unexpected change as an
  error, keeping in mind that this only narrows the window.
- Use the `secure` walker described below, which is not affected by this race.

## The `secure` walker

> **Experimental:** the `secure` walker is new and looking for feedback. Its API may still
> change, including in minor releases, until it is declared stable. Please share your use
> cases, problems and suggestions by [opening an issue][10].

The opt-in `secure` Cargo feature provides `async_walkdir::secure::WalkDir`, available on
Unix and Windows. It walks the tree with directory handles instead of paths, the same
technique used by `std` to fix [CVE-2022-21658][8]:

- every directory is opened *relative to its already opened parent*, by a single name,
  without following symlinks (`openat` with `O_NOFOLLOW` on Unix, `NtCreateFile` with a
  root directory handle and `FILE_OPEN_REPARSE_POINT` on Windows);
- a directory swapped for a symlink (or a junction) after being listed is detected: the
  walker yields an error for it, does not descend into it, and continues the walk;
- replacing a parent directory has no effect, since the walker only uses the handle it
  has already opened.

```toml
[dependencies]
async-walkdir = { version = "2", features = ["secure"] }
```

```rust
use async_walkdir::secure::WalkDir;
use futures_lite::future::block_on;
use futures_lite::io::AsyncReadExt;
use futures_lite::stream::StreamExt;

block_on(async {
    let mut entries = WalkDir::new("my_directory");
    while let Some(entry) = entries.next().await {
        let entry = entry?;
        if entry.file_type().await?.is_file() {
            // Opened relative to the parent directory handle, never following symlinks.
            let mut content = Vec::new();
            entry.open().await?.read_to_end(&mut content).await?;
        }
    }
    Ok::<_, Box<dyn std::error::Error>>(())
});
```

Entries also give access to their parent directory handle (`parent_fd` on Unix,
`parent_handle` on Windows), to act on them with other `*at` system calls instead of their
path. The walker's guarantees do not extend to what you do with these handles: resolving
`..` or a name made of several path components relative to them can leave the walked tree.

### Migrating from `WalkDir`

The `secure` walker is a separate type: `async_walkdir::WalkDir` is unchanged.

| `async_walkdir`               | `async_walkdir::secure`                                      |
|-------------------------------|--------------------------------------------------------------|
| `WalkDir::new(root)`          | `WalkDir::new(root)`, or `WalkDir::from_fd` / `from_handle`  |
| `WalkDir::filter(f)`          | `WalkDir::filter(f)`, with the same `Filtering` values       |
| `DirEntry::path()`            | `DirEntry::path()`, for display only: acting on it is racy   |
| `DirEntry::file_name()`       | `DirEntry::file_name()`                                      |
| `DirEntry::file_type()`       | `DirEntry::file_type()`, returning `secure::FileType`        |
| `DirEntry::metadata()` (follows symlinks) | `DirEntry::metadata()`, returning `secure::Metadata`, never following symlinks |
| `async_fs::File::open(entry.path())` | `DirEntry::open()`                                    |

Limitations:

- The root directory itself is resolved by path, following symlinks: it must be trusted.
  Use `WalkDir::from_fd` (Unix) or `WalkDir::from_handle` (Windows) to start from a handle.
- Paths of yielded entries are the ones seen when their parent was listed. Once the walk
  moves on, the tree may change: use `DirEntry::open` or the parent handle, not the path.
- Each directory being walked keeps two handles open, and each yielded entry keeps its
  parent directory open until it is dropped. Collecting all the entries of a large tree
  therefore keeps one handle per directory open and may exceed the limit of open files of
  the process (`ulimit -n` on Unix): process entries as they are yielded and drop them.

### Evolution with the standard library

The standard library does not expose directory handle APIs yet, so the `secure` walker
relies on [rustix][11] on Unix and on Windows APIs called through [windows-sys][12], which
requires `unsafe` code. Directory handles are being added to `std`
([rust-lang/rust#120426][13]). The `secure` walker will evolve along with them: its
implementation will move to `std` once they are stable, dropping these dependencies and the
`unsafe` code, and its API may be aligned with the `std` one. No `rustix` or `windows-sys`
type is exposed in its API, so that this move does not break your code.

[1]: https://docs.rs/walkdir
[2]: https://docs.rs/async-fs
[3]: https://docs.rs/blocking
[4]: https://docs.rs/futures-core
[5]: https://docs.rs/tokio
[6]: https://docs.rs/async-std
[7]: https://docs.rs/smol
[8]: https://blog.rust-lang.org/2022/01/20/cve-2022-21658.html
[9]: https://github.com/BurntSushi/walkdir/issues/209
[10]: https://github.com/ririsoft/async-walkdir/issues
[11]: https://docs.rs/rustix
[12]: https://docs.rs/windows-sys
[13]: https://github.com/rust-lang/rust/issues/120426
