// SPDX-FileCopyrightText: 2020-2024 Ririsoft <riri@ririsoft.com>
// SPDX-FileCopyrightText: 2024 Jordan Danford <jordandanford@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::{
    error, fmt, io,
    ops::Deref,
    path::{Path, PathBuf},
};

/// A wrapper around [`io::Error`] that includes the associated path.
#[derive(Debug)]
pub struct Error {
    path: PathBuf,
    inner: io::Error,
}

impl Error {
    /// Create a new [`Error`]
    pub fn new(path: PathBuf, inner: io::Error) -> Self {
        Error { path, inner }
    }

    /// Returns the path associated with this error.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Deref for Error {
    type Target = io::Error;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl error::Error for Error {
    #[allow(deprecated)]
    fn description(&self) -> &str {
        self.inner.description()
    }

    fn cause(&self) -> Option<&dyn error::Error> {
        self.source()
    }

    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        Some(&self.inner)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "IO error for operation on {}: {}",
            self.path.display(),
            self.inner
        )
    }
}

impl From<Error> for std::io::Error {
    fn from(err: Error) -> Self {
        err.inner
    }
}
