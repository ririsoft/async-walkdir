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
- Robust protection requires walking with directory handles (`openat` on Unix,
  relative `NtCreateFile` on Windows) instead of paths.

[1]: https://docs.rs/walkdir
[2]: https://docs.rs/async-fs
[3]: https://docs.rs/blocking
[4]: https://docs.rs/futures-core
[5]: https://docs.rs/tokio
[6]: https://docs.rs/async-std
[7]: https://docs.rs/smol
[8]: https://blog.rust-lang.org/2022/01/20/cve-2022-21658.html
[9]: https://github.com/BurntSushi/walkdir/issues/209
