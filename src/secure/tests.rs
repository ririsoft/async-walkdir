use std::io::{ErrorKind, Result};
use std::path::{Path, PathBuf};

use futures_lite::future::block_on;
use futures_lite::io::AsyncReadExt;
use futures_lite::stream::StreamExt;

use super::{DirEntry, WalkDir};
use crate::Filtering;

/// Walks `wd` to completion, returning the sorted paths of the yielded
/// entries and the paths of the yielded errors.
async fn collect(mut wd: WalkDir) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let (mut entries, mut errors) = (Vec::new(), Vec::new());
    while let Some(entry) = wd.next().await {
        match entry {
            Ok(entry) => entries.push(entry.path()),
            Err(e) => errors.push(e.path().unwrap().to_owned()),
        }
    }
    entries.sort();
    (entries, errors)
}

/// Creates `root/f1.txt`, `root/d1/f2.txt` and `root/d1/d2/f3.txt`.
async fn make_tree(root: &Path) -> Result<()> {
    async_fs::create_dir_all(root.join("d1").join("d2")).await?;
    async_fs::write(root.join("f1.txt"), "f1").await?;
    async_fs::write(root.join("d1").join("f2.txt"), "f2").await?;
    async_fs::write(root.join("d1").join("d2").join("f3.txt"), "f3").await?;
    Ok(())
}

async fn find(root: &Path, path: &Path) -> DirEntry {
    let mut wd = WalkDir::new(root);
    while let Some(entry) = wd.next().await {
        let entry = entry.unwrap();
        if entry.path() == path {
            return entry;
        }
    }
    panic!("{} not found", path.display());
}

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
                assert_eq!(e.path().unwrap(), Path::new("foobar"));
                assert_eq!(e.io().unwrap().kind(), ErrorKind::NotFound);
            }
            _ => panic!("want IO error"),
        }
        assert!(wd.next().await.is_none());
    })
}

#[test]
fn walk_dir_files() -> Result<()> {
    block_on(async {
        let root = tempfile::tempdir()?;
        let r = root.path();
        make_tree(r).await?;
        let (got, errors) = collect(WalkDir::new(r)).await;
        let want = vec![
            r.join("d1"),
            r.join("d1").join("d2"),
            r.join("d1").join("d2").join("f3.txt"),
            r.join("d1").join("f2.txt"),
            r.join("f1.txt"),
        ];
        assert_eq!(got, want);
        assert!(errors.is_empty());
        Ok(())
    })
}

#[test]
fn walk_dir_batch_boundaries() -> Result<()> {
    use std::collections::BTreeSet;

    block_on(async {
        // Around the size of a listing batch: every entry must be yielded
        // exactly once, whatever the number of batches.
        let batch = super::BATCH_SIZE;
        for count in [batch - 1, batch, batch + 1, 2 * batch + 1] {
            let root = tempfile::tempdir()?;
            let want: BTreeSet<_> = (0..count)
                .map(|i| root.path().join(i.to_string()))
                .collect();
            for path in &want {
                async_fs::write(path, []).await?;
            }
            let (got, errors) = collect(WalkDir::new(root.path())).await;
            assert!(errors.is_empty());
            assert_eq!(got.len(), count, "duplicate or missing entries");
            assert_eq!(got.into_iter().collect::<BTreeSet<_>>(), want);
        }
        Ok(())
    })
}

#[test]
fn filter_dirs() -> Result<()> {
    block_on(async {
        let root = tempfile::tempdir()?;
        let r = root.path();
        make_tree(r).await?;
        let wd = WalkDir::new(r).filter(|entry| async move {
            match entry.file_type().await {
                Ok(ft) if ft.is_dir() => Filtering::Ignore,
                _ => Filtering::Continue,
            }
        });
        let (got, _) = collect(wd).await;
        let want = vec![
            r.join("d1").join("d2").join("f3.txt"),
            r.join("d1").join("f2.txt"),
            r.join("f1.txt"),
        ];
        assert_eq!(got, want);
        Ok(())
    })
}

#[test]
fn filter_dirs_no_traverse() -> Result<()> {
    block_on(async {
        let root = tempfile::tempdir()?;
        let r = root.path();
        make_tree(r).await?;
        let d2 = r.join("d1").join("d2");
        let wd = WalkDir::new(r).filter(move |entry| {
            let d2 = d2.clone();
            async move {
                if entry.path() == d2 {
                    Filtering::IgnoreDir
                } else {
                    Filtering::Continue
                }
            }
        });
        let (got, _) = collect(wd).await;
        let want = vec![r.join("d1"), r.join("d1").join("f2.txt"), r.join("f1.txt")];
        assert_eq!(got, want);
        Ok(())
    })
}

#[test]
fn metadata_and_open() -> Result<()> {
    block_on(async {
        let root = tempfile::tempdir()?;
        make_tree(root.path()).await?;
        let entry = find(root.path(), &root.path().join("f1.txt")).await;
        assert_eq!(entry.file_name(), "f1.txt");
        let ft = entry.file_type().await?;
        assert!(ft.is_file() && !ft.is_dir() && !ft.is_symlink());
        let md = entry.metadata().await?;
        assert!(md.is_file());
        assert_eq!(md.len(), 2);
        assert!(!md.readonly());
        let modified = md.modified()?;
        let elapsed = modified.elapsed().unwrap_or_default();
        assert!(elapsed.as_secs() < 3600, "unexpected mtime {modified:?}");
        let mut content = String::new();
        entry.open().await?.read_to_string(&mut content).await?;
        assert_eq!(content, "f1");
        Ok(())
    })
}

#[test]
fn entry_outlives_walker() -> Result<()> {
    block_on(async {
        let root = tempfile::tempdir()?;
        make_tree(root.path()).await?;
        // `find` drops the walker: the entry keeps its parent directory open.
        let entry = find(root.path(), &root.path().join("d1").join("f2.txt")).await;
        let mut content = String::new();
        entry.open().await?.read_to_string(&mut content).await?;
        assert_eq!(content, "f2");
        Ok(())
    })
}

#[test]
fn entry_accessors() -> Result<()> {
    use std::collections::HashSet;
    use std::time::{Duration, UNIX_EPOCH};

    block_on(async {
        let root = tempfile::tempdir()?;
        make_tree(root.path()).await?;
        // A modification time before the Unix epoch, to exercise the
        // conversion of negative timestamps.
        let before_epoch = UNIX_EPOCH - Duration::from_secs(86_400);
        std::fs::File::options()
            .write(true)
            .open(root.path().join("f1.txt"))?
            .set_modified(before_epoch)?;

        let dir = find(root.path(), &root.path().join("d1")).await;
        assert!(format!("{dir:?}").contains("d1"));
        let dir_md = dir.metadata().await?;
        assert!(dir_md.is_dir() && !dir_md.is_file() && !dir_md.is_symlink());
        dir_md.accessed()?;
        // The creation time is not recorded everywhere.
        if let Err(e) = dir_md.created() {
            assert_eq!(e.kind(), ErrorKind::Unsupported);
        }

        let file = find(root.path(), &root.path().join("f1.txt")).await;
        let file_md = file.metadata().await?;
        assert_eq!(file_md.modified()?, before_epoch);

        let types: HashSet<_> = [dir_md.file_type(), file_md.file_type()].into();
        assert_eq!(types.len(), 2);
        Ok(())
    })
}

#[test]
fn walker_is_send() {
    fn assert_send<T: Send>(_: T) {}
    let wd = WalkDir::new("foo");
    assert_send(async move {
        let mut wd = wd;
        if let Some(Ok(entry)) = wd.next().await {
            let _ = entry.metadata().await;
            let _ = entry.open().await;
        }
    });
}

#[cfg(unix)]
mod unix {
    use std::io::Result;
    use std::os::unix::fs::{symlink, PermissionsExt};

    use futures_lite::future::block_on;
    use futures_lite::io::AsyncReadExt;
    use futures_lite::stream::StreamExt;
    use rustix::fs::{Mode, OFlags};

    use super::{collect, find, make_tree};
    use crate::secure::{FileTypeExt, MetadataExt, WalkDir};
    use crate::Filtering;

    #[test]
    fn symlink_is_not_followed() -> Result<()> {
        block_on(async {
            let root = tempfile::tempdir()?;
            let outside = tempfile::tempdir()?;
            async_fs::write(outside.path().join("secret"), "secret").await?;
            let link = root.path().join("link");
            symlink(outside.path(), &link)?;

            let (got, errors) = collect(WalkDir::new(root.path())).await;
            assert_eq!(got, vec![link.clone()]);
            assert!(errors.is_empty());

            let entry = find(root.path(), &link).await;
            assert!(entry.file_type().await?.is_symlink());
            // The metadata describes the link, not its target.
            assert!(entry.metadata().await?.is_symlink());
            assert!(entry.open().await.is_err());
            Ok(())
        })
    }

    /// A directory swapped for a symlink after being listed and before
    /// being opened: the walker must report it and not descend into it.
    #[test]
    fn swapped_dir_is_not_followed() -> Result<()> {
        block_on(async {
            let root = tempfile::tempdir()?;
            let outside = tempfile::tempdir()?;
            async_fs::write(outside.path().join("secret"), "secret").await?;
            let d1 = root.path().join("d1");
            async_fs::create_dir(&d1).await?;

            // The filter runs between the listing of `root` and the opening of `d1`.
            let target = outside.path().to_owned();
            let wd = WalkDir::new(root.path()).filter(move |entry| {
                let target = target.clone();
                async move {
                    std::fs::remove_dir(entry.path()).unwrap();
                    symlink(target, entry.path()).unwrap();
                    Filtering::Continue
                }
            });
            let (got, errors) = collect(wd).await;
            assert!(got.is_empty(), "walk escaped the root: {got:?}");
            assert_eq!(errors, vec![d1]);
            Ok(())
        })
    }

    /// A parent directory swapped for a symlink while being walked: the
    /// walker keeps reading the directory it has opened.
    #[test]
    fn swapped_parent_is_not_followed() -> Result<()> {
        block_on(async {
            let root = tempfile::tempdir()?;
            let r = root.path().to_owned();
            let outside = tempfile::tempdir()?;
            async_fs::create_dir_all(outside.path().join("d2")).await?;
            async_fs::write(outside.path().join("d2").join("secret"), "s").await?;
            make_tree(&r).await?;

            // When reaching `d1/d2`, `d1` is already open: move it away and
            // replace it with a symlink before `d2` gets opened.
            let d2 = r.join("d1").join("d2");
            let (root_path, target) = (r.clone(), outside.path().to_owned());
            let wd = WalkDir::new(&r).filter(move |entry| {
                let (d2, root_path, target) = (d2.clone(), root_path.clone(), target.clone());
                async move {
                    if entry.path() == d2 {
                        std::fs::rename(root_path.join("d1"), root_path.join("moved")).unwrap();
                        symlink(target, root_path.join("d1")).unwrap();
                    }
                    Filtering::Continue
                }
            });
            let (got, errors) = collect(wd).await;
            assert!(errors.is_empty());
            // `d2` is read from the original, moved, `d1`: the paths are the
            // ones from the time of the listing.
            assert!(got.contains(&r.join("d1").join("d2").join("f3.txt")));
            assert!(!got.iter().any(|p| p.ends_with("secret")), "{got:?}");
            Ok(())
        })
    }

    #[test]
    fn ignore_dir_does_not_read_it() -> Result<()> {
        block_on(async {
            let root = tempfile::tempdir()?;
            let d1 = root.path().join("d1");
            async_fs::create_dir(&d1).await?;
            std::fs::set_permissions(&d1, std::fs::Permissions::from_mode(0o222))?;
            let wd = WalkDir::new(root.path()).filter(|_| async { Filtering::IgnoreDir });
            let (got, errors) = collect(wd).await;
            assert!(got.is_empty() && errors.is_empty());
            Ok(())
        })
    }

    #[test]
    fn unreadable_dir_error_path() -> Result<()> {
        block_on(async {
            let root = tempfile::tempdir()?;
            let d1 = root.path().join("d1");
            async_fs::create_dir(&d1).await?;
            std::fs::set_permissions(&d1, std::fs::Permissions::from_mode(0o222))?;
            let (got, errors) = collect(WalkDir::new(root.path())).await;
            // Like `crate::WalkDir`, an unreadable directory is reported instead of yielded.
            assert!(got.is_empty());
            assert_eq!(errors, vec![d1]);
            Ok(())
        })
    }

    #[test]
    fn open_fifo_does_not_block() -> Result<()> {
        block_on(async {
            let root = tempfile::tempdir()?;
            let fifo = root.path().join("fifo");
            // `mknodat` is not available on every Unix: use the `mkfifo` utility.
            let status = std::process::Command::new("mkfifo").arg(&fifo).status()?;
            assert!(status.success());
            let entry = find(root.path(), &fifo).await;
            assert!(entry.file_type().await?.is_fifo());
            // Without `O_NONBLOCK`, opening a FIFO without writer would hang.
            let mut buf = Vec::new();
            entry.open().await?.read_to_end(&mut buf).await?;
            assert!(buf.is_empty());
            Ok(())
        })
    }

    #[test]
    fn parent_fd_and_metadata_ext() -> Result<()> {
        block_on(async {
            let root = tempfile::tempdir()?;
            make_tree(root.path()).await?;
            let path = root.path().join("f1.txt");
            let entry = find(root.path(), &path).await;
            let fd = rustix::fs::openat(
                entry.parent_fd(),
                entry.file_name().as_os_str(),
                OFlags::RDONLY | OFlags::CLOEXEC,
                Mode::empty(),
            )?;
            let st = rustix::fs::fstat(&fd)?;
            let md = entry.metadata().await?;
            assert_eq!(md.ino(), st.st_ino as u64);
            let std_md = std::fs::symlink_metadata(&path)?;
            assert_eq!(md.mode(), std::os::unix::fs::MetadataExt::mode(&std_md));
            assert_eq!(md.uid(), std::os::unix::fs::MetadataExt::uid(&std_md));
            assert_eq!(md.nlink(), 1);
            Ok(())
        })
    }

    #[test]
    fn file_type_ext() -> Result<()> {
        block_on(async {
            let root = tempfile::tempdir()?;
            make_tree(root.path()).await?;
            let entry = find(root.path(), &root.path().join("f1.txt")).await;
            let ft = entry.file_type().await?;
            assert!(!ft.is_fifo() && !ft.is_socket());
            assert!(!ft.is_block_device() && !ft.is_char_device());
            let md = entry.metadata().await?;
            let std_md = std::fs::symlink_metadata(entry.path())?;
            assert_eq!(md.dev(), std::os::unix::fs::MetadataExt::dev(&std_md));
            assert_eq!(md.gid(), std::os::unix::fs::MetadataExt::gid(&std_md));
            Ok(())
        })
    }

    #[test]
    fn from_fd_not_a_dir() -> Result<()> {
        block_on(async {
            let root = tempfile::tempdir()?;
            make_tree(root.path()).await?;
            let file = std::fs::File::open(root.path().join("f1.txt"))?;
            let mut wd = WalkDir::from_fd(file.into());
            match wd.next().await {
                Some(Err(e)) => assert_eq!(e.path().unwrap(), std::path::Path::new("")),
                other => panic!("want an error, got {other:?}"),
            }
            assert!(wd.next().await.is_none());
            Ok(())
        })
    }

    #[test]
    fn from_fd_yields_relative_paths() -> Result<()> {
        block_on(async {
            let root = tempfile::tempdir()?;
            make_tree(root.path()).await?;
            let fd = rustix::fs::open(
                root.path(),
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
                Mode::empty(),
            )?;
            let (got, errors) = collect(WalkDir::from_fd(fd)).await;
            assert!(errors.is_empty());
            assert!(got.contains(&"f1.txt".into()));
            assert!(got.contains(&std::path::Path::new("d1").join("d2").join("f3.txt")));
            assert_eq!(got.len(), 5);
            Ok(())
        })
    }
}

#[cfg(windows)]
mod windows {
    use std::io::{Error, Result};
    use std::path::Path;
    use std::process::Command;

    use futures_lite::future::block_on;
    use futures_lite::stream::StreamExt;

    use super::{collect, find, make_tree};
    use crate::secure::{MetadataExt, WalkDir};
    use crate::Filtering;

    /// Creates a directory junction, which unlike symbolic links requires no
    /// privilege.
    fn junction(link: &Path, target: &Path) -> Result<()> {
        let status = Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .status()?;
        if !status.success() {
            return Err(Error::other(format!("mklink /J failed: {status}")));
        }
        Ok(())
    }

    #[test]
    fn junction_is_not_followed() -> Result<()> {
        block_on(async {
            let root = tempfile::tempdir()?;
            let outside = tempfile::tempdir()?;
            async_fs::write(outside.path().join("secret"), "secret").await?;
            let link = root.path().join("link");
            junction(&link, outside.path())?;

            let (got, errors) = collect(WalkDir::new(root.path())).await;
            assert_eq!(got, vec![link.clone()]);
            assert!(errors.is_empty());

            let entry = find(root.path(), &link).await;
            assert!(entry.file_type().await?.is_symlink());
            assert!(entry.metadata().await?.is_symlink());
            assert!(entry.open().await.is_err());
            Ok(())
        })
    }

    #[test]
    fn symlink_is_not_followed() -> Result<()> {
        block_on(async {
            let root = tempfile::tempdir()?;
            let outside = tempfile::tempdir()?;
            async_fs::write(outside.path().join("secret"), "secret").await?;
            let link = root.path().join("link");
            match std::os::windows::fs::symlink_dir(outside.path(), &link) {
                // ERROR_PRIVILEGE_NOT_HELD: symbolic links require the
                // developer mode or an elevated process.
                Err(e) if e.raw_os_error() == Some(1314) => return Ok(()),
                r => r?,
            }
            let (got, errors) = collect(WalkDir::new(root.path())).await;
            assert_eq!(got, vec![link]);
            assert!(errors.is_empty());
            Ok(())
        })
    }

    /// A directory swapped for a junction after being listed and before
    /// being opened: the walker must report it and not descend into it.
    #[test]
    fn swapped_dir_is_not_followed() -> Result<()> {
        block_on(async {
            let root = tempfile::tempdir()?;
            let outside = tempfile::tempdir()?;
            async_fs::write(outside.path().join("secret"), "secret").await?;
            let d1 = root.path().join("d1");
            async_fs::create_dir(&d1).await?;

            // The filter runs between the listing of `root` and the opening of `d1`.
            let target = outside.path().to_owned();
            let wd = WalkDir::new(root.path()).filter(move |entry| {
                let target = target.clone();
                async move {
                    std::fs::remove_dir(entry.path()).unwrap();
                    junction(&entry.path(), &target).unwrap();
                    Filtering::Continue
                }
            });
            let (got, errors) = collect(wd).await;
            assert!(got.is_empty(), "walk escaped the root: {got:?}");
            assert_eq!(errors, vec![d1]);
            Ok(())
        })
    }

    #[test]
    fn open_dir_is_refused() -> Result<()> {
        block_on(async {
            let root = tempfile::tempdir()?;
            make_tree(root.path()).await?;
            let entry = find(root.path(), &root.path().join("d1")).await;
            assert!(entry.open().await.is_err());
            Ok(())
        })
    }

    #[test]
    fn parent_handle_and_metadata_ext() -> Result<()> {
        block_on(async {
            // `FILE_ATTRIBUTE_READONLY`.
            const READONLY: u32 = 0x1;

            let root = tempfile::tempdir()?;
            make_tree(root.path()).await?;
            let path = root.path().join("f1.txt");
            let mut perms = std::fs::metadata(&path)?.permissions();
            perms.set_readonly(true);
            std::fs::set_permissions(&path, perms)?;

            let entry = find(root.path(), &path).await;
            let md = entry.metadata().await?;
            assert!(md.readonly());
            assert_ne!(md.file_attributes() & READONLY, 0);
            assert_ne!(md.file_index(), 0);

            let parent = std::fs::File::from(entry.parent_handle().try_clone_to_owned()?);
            assert!(parent.metadata()?.is_dir());
            Ok(())
        })
    }

    #[test]
    fn from_handle_not_a_dir() -> Result<()> {
        block_on(async {
            let root = tempfile::tempdir()?;
            make_tree(root.path()).await?;
            let file = std::fs::File::open(root.path().join("f1.txt"))?;
            let mut wd = WalkDir::from_handle(file.into());
            match wd.next().await {
                Some(Err(e)) => assert_eq!(e.path().unwrap(), Path::new("")),
                other => panic!("want an error, got {other:?}"),
            }
            assert!(wd.next().await.is_none());
            Ok(())
        })
    }

    #[test]
    fn from_handle_yields_relative_paths() -> Result<()> {
        use std::os::windows::fs::OpenOptionsExt;
        // `FILE_FLAG_BACKUP_SEMANTICS`, required to open a directory.
        const BACKUP_SEMANTICS: u32 = 0x0200_0000;

        block_on(async {
            let root = tempfile::tempdir()?;
            make_tree(root.path()).await?;
            let dir = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(BACKUP_SEMANTICS)
                .open(root.path())?;
            let (got, errors) = collect(WalkDir::from_handle(dir.into())).await;
            assert!(errors.is_empty());
            assert!(got.contains(&"f1.txt".into()));
            assert!(got.contains(&Path::new("d1").join("d2").join("f3.txt")));
            assert_eq!(got.len(), 5);
            Ok(())
        })
    }

    #[test]
    fn root_file_is_refused() -> Result<()> {
        block_on(async {
            let root = tempfile::tempdir()?;
            make_tree(root.path()).await?;
            let file = root.path().join("f1.txt");
            let (got, errors) = collect(WalkDir::new(&file)).await;
            assert!(got.is_empty());
            assert_eq!(errors, vec![file]);
            Ok(())
        })
    }
}
