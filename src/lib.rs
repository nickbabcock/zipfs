//! A read-only FUSE filesystem for zip archives.
//!
//! The crate is layered so that most of it is testable without a mount. Only
//! [`fs`] and the binary need FUSE; [`index`], [`source`], [`codec`], [`decode`]
//! and [`pool`] operate on a plain archive.

pub mod archive;
pub mod attr;
pub mod codec;
pub mod config;
pub mod decode;
pub mod fs;
pub mod handle;
pub mod hash;
pub mod index;
pub mod pool;
pub mod source;

pub use archive::Archive;
pub use config::Config;
pub use fs::ZipFs;
pub use index::{Index, Node, NodeKind};

use std::fmt;

/// Errors that can occur while opening an archive or serving a request.
#[derive(Debug)]
pub enum Error {
    /// The archive could not be opened or parsed.
    Zip(rawzip::Error),
    /// An I/O error from the underlying file.
    Io(std::io::Error),
    /// The entry uses encryption, which is not supported.
    Encrypted,
    /// The entry uses a compression method that is not supported.
    Unsupported(u16),
    /// The decompressed data did not match the central directory.
    Corrupt(&'static str),
    /// The archive is too large for the in-memory index.
    IndexTooLarge(&'static str),
}

impl Error {
    /// The errno to report to the kernel for this error.
    #[must_use]
    pub fn errno(&self) -> i32 {
        match self {
            Error::Encrypted => libc::EACCES,
            Error::Unsupported(_) => libc::EOPNOTSUPP,
            Error::Io(e) => e.raw_os_error().unwrap_or(libc::EIO),
            Error::Zip(_) | Error::Corrupt(_) => libc::EIO,
            Error::IndexTooLarge(_) => libc::EOVERFLOW,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Zip(e) => write!(f, "zip error: {e}"),
            Error::Io(e) => write!(f, "io error: {e}"),
            Error::Encrypted => write!(f, "entry is encrypted"),
            Error::Unsupported(m) => write!(f, "unsupported compression method {m}"),
            Error::Corrupt(what) => write!(f, "corrupt entry: {what}"),
            Error::IndexTooLarge(what) => write!(f, "archive is too large for index: {what}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<rawzip::Error> for Error {
    fn from(e: rawzip::Error) -> Self {
        Error::Zip(e)
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;
