//! Mounts a zip archive as a read-only filesystem.

use fuser::{MountOption, Session, SessionACL};
use log::{Level, LevelFilter, Log, Metadata, Record};
use std::error::Error;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;
use zipfs::{Archive, Config, ZipFs};

// Avoid the default musl allocator under concurrent allocation workloads.
// https://nickb.dev/blog/default-musl-allocator-considered-harmful-to-performance/
#[cfg(target_env = "musl")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

const USAGE: &str = "\
zipfs - mount a zip archive as a read-only filesystem

Usage: zipfs [OPTIONS] <ARCHIVE> <MOUNTPOINT>

Options:
      --threads N               FUSE worker threads [default: cores, up to 8]
      --no-verify               skip the CRC32 check on entries that are read
                                to the end
      --source-buffer-size N    compressed bytes each decoder buffers
                                [default: 524288]
      --decoders-per-file N     idle decoders one open file may keep [default: 4]
      --max-retained-decoders N idle decoders the mount may keep [default: 64]
      --uid ID                  owner reported for every file [default: caller]
      --gid ID                  group reported for every file [default: caller]
      --file-mode MODE          permissions for files whose entry records none
                                [default: 644]
      --dir-mode MODE           permissions for directories whose entry records
                                none [default: 755]
      --attr-ttl SECS           how long the kernel may cache metadata
                                [default: 31536000]
      --allow-other             let other users see the mount
      --auto-unmount            unmount if this process dies; needs --allow-other
  -v, --verbose                 log more; repeat for debug and trace
  -h, --help                    print this help
  -V, --version                 print the version
";

/// Writes log records to standard error.
struct Stderr;

impl Log for Stderr {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let level = match record.level() {
            Level::Error => "error",
            Level::Warn => "warning",
            Level::Info => "info",
            Level::Debug => "debug",
            Level::Trace => "trace",
        };
        eprintln!("zipfs: {level}: {}", record.args());
    }

    fn flush(&self) {}
}

static LOGGER: Stderr = Stderr;

#[derive(Debug)]
struct ContextError {
    context: String,
    source: Box<dyn Error>,
}

impl ContextError {
    fn new<E>(context: impl Into<String>, source: E) -> ContextError
    where
        E: Error + 'static,
    {
        ContextError {
            context: context.into(),
            source: Box::new(source),
        }
    }
}

impl std::fmt::Display for ContextError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.context, self.source)
    }
}

impl Error for ContextError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.source.as_ref())
    }
}

struct Args {
    archive: PathBuf,
    mountpoint: PathBuf,
    config: Config,
    verbosity: u8,
}

fn parse() -> Result<Option<Args>, lexopt::Error> {
    use lexopt::prelude::*;

    let mut archive = None;
    let mut mountpoint = None;
    let mut config = Config::default();
    let mut verbosity = 0u8;
    let mut auto_unmount = false;
    let mut parser = lexopt::Parser::from_env();

    while let Some(arg) = parser.next()? {
        match arg {
            Long("threads") => config.threads = parser.value()?.parse()?,
            Long("no-verify") => config.verify = false,
            Long("source-buffer-size") => config.source_buffer = parser.value()?.parse()?,
            Long("decoders-per-file") => config.decoders_per_file = parser.value()?.parse()?,
            Long("max-retained-decoders") => {
                config.max_retained_decoders = parser.value()?.parse()?;
            }
            Long("uid") => config.uid = parser.value()?.parse()?,
            Long("gid") => config.gid = parser.value()?.parse()?,
            Long("file-mode") => config.file_mode = parse_mode(&parser.value()?)?,
            Long("dir-mode") => config.dir_mode = parse_mode(&parser.value()?)?,
            Long("attr-ttl") => config.attr_ttl = Duration::from_secs(parser.value()?.parse()?),
            Long("allow-other") => config.allow_other = true,
            Long("auto-unmount") => auto_unmount = true,
            Short('v') | Long("verbose") => verbosity = verbosity.saturating_add(1),
            Short('h') | Long("help") => {
                print!("{USAGE}");
                return Ok(None);
            }
            Short('V') | Long("version") => {
                println!("zipfs {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            Value(v) if archive.is_none() => archive = Some(PathBuf::from(v)),
            Value(v) if mountpoint.is_none() => mountpoint = Some(PathBuf::from(v)),
            _ => return Err(arg.unexpected()),
        }
    }

    config.auto_unmount = auto_unmount;
    let (Some(archive), Some(mountpoint)) = (archive, mountpoint) else {
        return Err(lexopt::Error::MissingValue {
            option: Some("ARCHIVE and MOUNTPOINT".into()),
        });
    };
    Ok(Some(Args {
        archive,
        mountpoint,
        config,
        verbosity,
    }))
}

fn parse_mode(value: &std::ffi::OsString) -> Result<u16, lexopt::Error> {
    let text = value.to_string_lossy();
    u16::from_str_radix(text.trim_start_matches("0o"), 8).map_err(|error| {
        lexopt::Error::ParsingFailed {
            value: text.into_owned(),
            error: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("expected an octal mode such as 644: {error}"),
            )
            .into(),
        }
    })
}

fn main() -> ExitCode {
    let args = match parse() {
        Ok(Some(args)) => args,
        Ok(None) => return ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("zipfs: {e}\n\n{USAGE}");
            return ExitCode::FAILURE;
        }
    };

    let _ = log::set_logger(&LOGGER);
    log::set_max_level(match args.verbosity {
        0 => LevelFilter::Warn,
        1 => LevelFilter::Info,
        2 => LevelFilter::Debug,
        _ => LevelFilter::Trace,
    });

    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("zipfs: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    if args.config.auto_unmount && !args.config.allow_other {
        // FUSE will not accept the combination, so say why rather than let the
        // mount fail with the kernel's wording.
        return Err("--auto-unmount needs --allow-other".into());
    }

    let mountpoint = match fs::metadata(&args.mountpoint) {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "mount point '{}' is not a directory",
                args.mountpoint.display()
            ),
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(std::io::Error::new(
            e.kind(),
            format!(
                "mount point '{}' does not exist; create it first",
                args.mountpoint.display()
            ),
        )),
        Err(e) => Err(std::io::Error::new(
            e.kind(),
            format!(
                "cannot access mount point '{}': {e}",
                args.mountpoint.display()
            ),
        )),
    };
    mountpoint.map_err(|e| Box::new(e) as Box<dyn std::error::Error>)?;

    let archive = Archive::open(&args.archive).map_err(|e| {
        ContextError::new(
            format!("cannot open archive '{}'", args.archive.display()),
            e,
        )
    })?;
    let fs = ZipFs::new(archive, args.config.clone()).map_err(|e| {
        ContextError::new(
            format!("cannot index archive '{}'", args.archive.display()),
            e,
        )
    })?;
    let config = fs.config().clone();
    report(fs.stats());

    let mut options = vec![
        MountOption::RO,
        MountOption::NoAtime,
        MountOption::NoSuid,
        MountOption::NoDev,
        MountOption::FSName(args.archive.display().to_string()),
        MountOption::Subtype("zipfs".to_string()),
        MountOption::DefaultPermissions,
    ];
    if config.auto_unmount {
        options.push(MountOption::AutoUnmount);
    }

    let mut session_config = fuser::Config::default();
    session_config.mount_options = options;
    // Who may reach the mount is set here rather than through a mount option.
    // `auto_unmount` needs it to be more than the owner.
    session_config.acl = if config.allow_other {
        SessionACL::All
    } else {
        SessionACL::Owner
    };
    session_config.n_threads = Some(config.threads);
    // Each worker gets its own descriptor on the fuse device, which is what
    // lets the threads scale past two or three.
    session_config.clone_fd = true;

    log::info!(
        "mounting {} on {} with {} threads",
        args.archive.display(),
        args.mountpoint.display(),
        config.threads
    );
    let session = Session::new(fs, &args.mountpoint, &session_config).map_err(|e| {
        ContextError::new(
            format!(
                "cannot mount archive '{}' at '{}'",
                args.archive.display(),
                args.mountpoint.display()
            ),
            e,
        )
    })?;
    session.run().map_err(|e| {
        ContextError::new(
            format!(
                "mount at '{}' stopped with an error",
                args.mountpoint.display()
            ),
            e,
        )
    })?;
    Ok(())
}

fn report(stats: &zipfs::index::BuildStats) {
    log::info!(
        "indexed {} entries and {} implied directories",
        stats.entries,
        stats.synthetic_dirs
    );
    if stats.rejected_paths > 0 {
        log::warn!("{} entries had an unusable path", stats.rejected_paths);
    }
    if stats.rejected_sizes > 0 {
        log::warn!(
            "{} entries had an untrustworthy compressed size",
            stats.rejected_sizes
        );
    }
    if stats.duplicates > 0 {
        log::warn!(
            "{} entries lost their name to an earlier one",
            stats.duplicates
        );
    }
    if stats.encrypted > 0 {
        log::warn!(
            "{} entries are encrypted and cannot be read",
            stats.encrypted
        );
    }
    if stats.unsupported > 0 {
        log::warn!(
            "{} entries use a compression method that is not supported",
            stats.unsupported
        );
    }
}
