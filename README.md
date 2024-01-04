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

[1]: https://docs.rs/walkdir
[2]: https://docs.rs/async-fs
[3]: https://docs.rs/blocking
[4]: https://docs.rs/futures-core
[5]: https://docs.rs/tokio
[6]: https://docs.rs/async-std
[7]: https://docs.rs/smol
