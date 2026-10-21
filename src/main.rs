//! Mounts a zip archive as a read-only filesystem.

mod cli;

use fuser::{MountOption, Session, SessionACL};
use log::{Level, LevelFilter, Log, Metadata, Record};
use std::cell::{Cell, RefCell};
use std::error::Error;
use std::fs;
use std::io::Write;
use std::os::unix::net::UnixDatagram;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::time::Duration;
use zipfs::{Archive, Config, ZipFs};

// Avoid the default musl allocator under concurrent allocation workloads.
// https://nickb.dev/blog/default-musl-allocator-considered-harmful-to-performance/
#[cfg(target_env = "musl")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

const USAGE_HEAD: &str = "\
zipfs - mount a zip archive as a read-only filesystem

Usage: zipfs [OPTIONS] <ARCHIVE> <MOUNTPOINT>

Options:
";

// This literal starts on the quote's own line: a backslash before the newline
// would take the indent of the first option with it.
const USAGE_TAIL: &str = "  -o OPT[,OPT...]               any option above, without the dashes, as
                                mount(8) and fstab spell it
  -f, --foreground              stay in the foreground and keep logging to
                                standard error, not to syslog
  -s                            serve with one thread, the same as --threads 1
  -v, --verbose                 log more; repeat for debug and trace
  -h, --help                    print this help
  -V, --version                 print the version

Without --foreground the process forks once the mount is ready and the first
process exits, so a script can mount and then read without waiting. After the
fork, logs go to syslog as 'zipfs'; see them with 'journalctl -t zipfs'.

Started under a name that begins with 'mount.', as mount(8) starts a helper,
the short options take their mount(8) meanings instead: -s tolerates unknown
options, -f does everything but the mount, -n is accepted and does nothing,
and -t names the type. Use --foreground and --threads to reach what -f and -s
mean otherwise.
";

/// The help, with the settings written out from the one list of them.
fn usage() -> String {
    format!("{USAGE_HEAD}{}{USAGE_TAIL}", cli::settings_help())
}

/// Writes log records to standard error, or to syslog once the process is a
/// daemon.
struct Logger;

/// Set after the process detaches. Standard error then goes to `/dev/null`,
/// so syslog is the only place a record can go.
static SYSLOG: OnceLock<Syslog> = OnceLock::new();

/// The number of records that syslog did not take and that the logger has
/// not yet reported.
static DROPPED: AtomicUsize = AtomicUsize::new(0);

/// The second, since the epoch, of the last report of dropped records. The
/// logger reports at most once each second, so that the reports do not fill
/// the queue that they report on.
static REPORTED: AtomicI64 = AtomicI64::new(0);

impl Log for Logger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        if let Some(syslog) = SYSLOG.get() {
            syslog.log(record);
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

static LOGGER: Logger = Logger;

/// Sends log records and panic reports to syslog from here on.
///
/// Call it after the process detaches and before the workers start. On a
/// systemd machine, syslog is the journal, and `journalctl -t zipfs` shows
/// the records.
fn redirect_to_syslog() {
    // Without the socket, records go to `/dev/null`, which is no worse than
    // before the process detached.
    let Ok(syslog) = Syslog::open(PathBuf::from("/dev/log")) else {
        return;
    };
    let _ = SYSLOG.set(syslog);
    // The default hook writes to standard error, so a panic on a worker
    // thread must go through the logger, or nobody sees it.
    std::panic::set_hook(Box::new(|info| {
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("<unnamed>");
        let message = info.payload_as_str().unwrap_or("Box<dyn Any>");
        let location = info
            .location()
            .map_or_else(String::new, |l| format!(" at {l}"));
        let backtrace = std::backtrace::Backtrace::capture();
        if backtrace.status() == std::backtrace::BacktraceStatus::Captured {
            log::error!("thread '{name}' panicked{location}: {message}\n{backtrace}");
        } else {
            log::error!("thread '{name}' panicked{location}: {message}");
        }
    }));
}

/// Two datagram sockets that send records to the syslog daemon at
/// `/dev/log`.
///
/// `syslog(3)` holds a lock for each record and waits while the daemon is
/// busy, so at debug levels all workers wait on the journal. Here each line
/// is one `sendto` with no lock. A line that the daemon cannot take at once
/// is dropped, and a later record tells how many were dropped.
///
/// Each send names the address and does not connect, so the sockets continue
/// to work after the daemon restarts.
struct Syslog {
    /// Waits for a busy daemon, but not longer than one second.
    waiting: UnixDatagram,
    /// Does not wait.
    quick: UnixDatagram,
    path: PathBuf,
    pid: u32,
}

thread_local! {
    /// The last second that this thread formatted, and its RFC 3164
    /// timestamp. `localtime_r` takes a lock in libc, so each thread calls
    /// it at most once each second.
    static STAMP: Cell<(libc::time_t, [u8; 15])> = const { Cell::new((-1, [0; 15])) };
}

thread_local! {
    /// The datagram for each thread, so that most records do not allocate.
    static BUFFER: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// The capacity that a thread's datagram keeps after a long record.
const BUFFER_CAPACITY: usize = 4096;

impl Syslog {
    fn open(path: PathBuf) -> std::io::Result<Syslog> {
        let waiting = UnixDatagram::unbound()?;
        waiting.set_write_timeout(Some(Duration::from_secs(1)))?;
        let quick = UnixDatagram::unbound()?;
        quick.set_nonblocking(true)?;
        Ok(Syslog {
            waiting,
            quick,
            path,
            pid: std::process::id(),
        })
    }

    fn log(&self, record: &Record) {
        let sent = BUFFER.try_with(|buffer| {
            // The buffer is in use when the panic hook runs while this thread
            // formats a record. Then the inner record takes a new buffer.
            let Ok(mut buffer) = buffer.try_borrow_mut() else {
                return false;
            };
            self.send_record(&mut buffer, record);
            // A long backtrace does not keep its memory for the thread's life.
            buffer.shrink_to(BUFFER_CAPACITY);
            true
        });
        if !matches!(sent, Ok(true)) {
            self.send_record(&mut Vec::new(), record);
        }
    }

    fn send_record(&self, datagram: &mut Vec<u8>, record: &Record) {
        let severity = match record.level() {
            Level::Error => libc::LOG_ERR,
            Level::Warn => libc::LOG_WARNING,
            Level::Info => libc::LOG_INFO,
            Level::Debug | Level::Trace => libc::LOG_DEBUG,
        };
        // An error is rare and is the record most likely to matter, so it
        // waits for a busy daemon. Other records do not stall the workers.
        let mut wait = record.level() == Level::Error;
        // SAFETY: `time` accepts a null pointer.
        let now = unsafe { libc::time(std::ptr::null_mut()) };

        datagram.clear();
        self.header(datagram, severity, now);
        let header = datagram.len();
        let _ = write!(datagram, "{}", record.args());
        let mut sent = false;
        // Some syslog daemons escape or cut a message at a newline, so send
        // each line as a record. The journal cuts a message at a NUL, so
        // remove each NUL. Most records have neither, and go as they are.
        if datagram[header..].iter().any(|&b| b == b'\n' || b == 0) {
            let message = datagram.split_off(header);
            for line in message
                .split(|&b| b == b'\n')
                .filter(|line| !line.is_empty())
            {
                datagram.truncate(header);
                datagram.extend(line.iter().filter(|&&b| b != 0));
                if self.send(datagram, wait) {
                    sent = true;
                } else {
                    DROPPED.fetch_add(1, Ordering::Relaxed);
                    // The daemon did not take this line in a second, so the
                    // other lines of a backtrace do not wait a second each.
                    wait = false;
                }
            }
        } else if datagram.len() > header {
            if self.send(datagram, wait) {
                sent = true;
            } else {
                DROPPED.fetch_add(1, Ordering::Relaxed);
            }
        }

        // The daemon took a line, so it can probably take the report too.
        if !sent || DROPPED.load(Ordering::Relaxed) == 0 {
            return;
        }
        let last = REPORTED.load(Ordering::Relaxed);
        if last == now
            || REPORTED
                .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_err()
        {
            return;
        }
        let dropped = DROPPED.swap(0, Ordering::Relaxed);
        if dropped > 0 {
            datagram.clear();
            self.header(datagram, libc::LOG_WARNING, now);
            let _ = write!(datagram, "syslog did not take {dropped} log lines");
            if !self.send(datagram, false) {
                DROPPED.fetch_add(dropped, Ordering::Relaxed);
            }
        }
    }

    /// Writes the RFC 3164 header, as `syslog(3)` writes it.
    fn header(&self, datagram: &mut Vec<u8>, severity: libc::c_int, now: libc::time_t) {
        let (second, mut stamp) = STAMP.get();
        if second != now {
            stamp = timestamp(now);
            STAMP.set((now, stamp));
        }
        let _ = write!(datagram, "<{}>", libc::LOG_DAEMON | severity);
        datagram.extend_from_slice(&stamp);
        let _ = write!(datagram, " zipfs[{}]: ", self.pid);
    }

    /// Sends one datagram, and tells if the daemon took it.
    fn send(&self, datagram: &[u8], wait: bool) -> bool {
        let socket = if wait { &self.waiting } else { &self.quick };
        loop {
            match socket.send_to(datagram, &self.path) {
                Ok(_) => return true,
                // A signal stopped the wait, so try again.
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => return false,
            }
        }
    }
}

/// Formats the local time as RFC 3164 does, for example `Oct  2 21:41:39`.
fn timestamp(now: libc::time_t) -> [u8; 15] {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    // SAFETY: all zeros is a valid `tm` for `localtime_r` to fill.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&raw const now, &raw mut tm);
        tm
    };
    let month = usize::try_from(tm.tm_mon)
        .ok()
        .and_then(|m| MONTHS.get(m))
        .unwrap_or(&MONTHS[0]);
    let mut stamp = [b' '; 15];
    let _ = write!(
        &mut stamp[..],
        "{month} {:2} {:02}:{:02}:{:02}",
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
    );
    stamp
}

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
    foreground: bool,
    /// Do everything except the mount itself, which is what `mount -f` asks
    /// a helper for.
    fake: bool,
}

fn parse() -> Result<Option<Args>, lexopt::Error> {
    use lexopt::prelude::*;

    let mut archive = None;
    let mut mountpoint = None;
    let mut config = Config::default();
    let mut verbosity = 0u8;
    let mut foreground = false;
    let mut fake = false;
    // mount(8) starts a helper as `mount.<type> spec dir [-sfnv] [-N ns]
    // [-o opts] [-t type]`, where -s, -f and -n mean something other than what
    // they mean here. Which set applies depends on the name this was started
    // under, the way other mount helpers decide it.
    let helper = started_as_mount_helper();
    let mut parser = lexopt::Parser::from_env();

    while let Some(arg) = parser.next()? {
        match arg {
            // The option list from mount(8) is applied where it appears, so a
            // long option after it still wins.
            Short('o') => {
                cli::apply(&mut config, &parser.value()?).map_err(lexopt::Error::from)?;
            }
            // Everything but the mount, so the caller learns whether the
            // archive and the mount point are fit for one.
            Short('f') if helper => fake = true,
            Short('f') | Long("foreground") => foreground = true,
            // -s is sloppy, which an option list is here in any case, and
            // -n asks for the mount table to be left alone, which mount(8)
            // never gives this program a part in.
            Short('s' | 'n') if helper => {}
            Short('s') => config.threads = 1,
            // The type is what started this program, so there is nothing left
            // to learn from it.
            Short('t') if helper => {
                parser.value()?;
            }
            Short('N') if helper => {
                return Err(lexopt::Error::from(String::from(
                    "mounting into another namespace is not supported",
                )));
            }
            Short('v') | Long("verbose") => verbosity = verbosity.saturating_add(1),
            Short('h') | Long("help") => {
                print!("{}", usage());
                return Ok(None);
            }
            Short('V') | Long("version") => {
                println!("zipfs {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            // Every setting is named in one place, and this is where a long
            // option reaches it.
            Long(name) => {
                let Some(setting) = cli::find(name) else {
                    return Err(lexopt::Error::from(format!("invalid option '--{name}'")));
                };
                cli::apply_long(setting, &mut config, &mut parser)?;
            }
            Value(v) if archive.is_none() => archive = Some(PathBuf::from(v)),
            Value(v) if mountpoint.is_none() => mountpoint = Some(PathBuf::from(v)),
            _ => return Err(arg.unexpected()),
        }
    }

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
        foreground,
        fake,
    }))
}

/// Reports whether a mount can be put here.
fn check_mountpoint(mountpoint: &std::path::Path) -> std::io::Result<()> {
    match fs::metadata(mountpoint) {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("mount point '{}' is not a directory", mountpoint.display()),
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(std::io::Error::new(
            e.kind(),
            format!(
                "mount point '{}' does not exist; create it first",
                mountpoint.display()
            ),
        )),
        Err(e) => Err(std::io::Error::new(
            e.kind(),
            format!("cannot access mount point '{}': {e}", mountpoint.display()),
        )),
    }
}

/// Whether mount(8) started this program as a helper.
///
/// A helper is reached through a name such as `mount.fuse.zipfs`, and mount(8)
/// gives it the full path in the first argument.
fn started_as_mount_helper() -> bool {
    std::env::args_os()
        .next()
        .map(PathBuf::from)
        .as_deref()
        .and_then(std::path::Path::file_name)
        .is_some_and(|name| name.as_encoded_bytes().starts_with(b"mount."))
}

fn main() -> ExitCode {
    // Parsing an option list can have something to say about it, so the logger
    // goes in first and the level it was asked for follows.
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(LevelFilter::Warn);
    let args = match parse() {
        Ok(Some(args)) => args,
        Ok(None) => return ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("zipfs: {e}\n\n{}", usage());
            return ExitCode::FAILURE;
        }
    };

    log::set_max_level(match args.verbosity {
        0 => LevelFilter::Warn,
        1 => LevelFilter::Info,
        2 => LevelFilter::Debug,
        _ => LevelFilter::Trace,
    });

    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // After the process detaches, standard error goes to `/dev/null`.
            if SYSLOG.get().is_some() {
                log::error!("{e}");
            } else {
                eprintln!("zipfs: {e}");
            }
            ExitCode::FAILURE
        }
    }
}

fn run(args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    if args.config.auto_unmount && !args.config.allow_other && !args.config.allow_root {
        // FUSE will not accept the combination, so say why rather than let the
        // mount fail with the kernel's wording.
        return Err("--auto-unmount needs --allow-other or --allow-root".into());
    }

    check_mountpoint(&args.mountpoint).map_err(|e| Box::new(e) as Box<dyn std::error::Error>)?;

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

    if args.fake {
        // The archive is open and indexed and the mount point has been
        // checked, which is as far as a dry run goes.
        log::info!(
            "'{}' can be mounted at '{}'",
            args.archive.display(),
            args.mountpoint.display()
        );
        return Ok(());
    }

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
    } else if config.allow_root {
        SessionACL::RootAndOwner
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
    // The mount answered the kernel's init request inside `Session::new`, so
    // it is ready to serve. Detaching here, and no earlier, is what lets the
    // caller treat this process leaving as the mount being usable.
    if !args.foreground {
        match daemonize() {
            Ok(()) => {}
            Err(DetachError::Local(e)) => {
                return Err(ContextError::new("cannot detach from the terminal", e).into());
            }
            // The first process has already told the caller and left with a
            // failing code. Nothing waits on this one, so all that is left is
            // to drop the session, which takes the mount with it.
            Err(DetachError::Reported) => return Ok(()),
        }
        redirect_to_syslog();
    }

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

/// The byte the child sends once the mount is its own to serve.
const READY: u8 = 0;

/// Why a mount is still in the foreground.
#[derive(Debug)]
enum DetachError {
    /// The fork never happened, so this process is the only one there is and
    /// the caller has heard nothing yet.
    Local(std::io::Error),
    /// The child could not detach and said so down the pipe. The first process
    /// reported it, so saying it again would only repeat it.
    Reported,
}

/// Forks, leaving the child to serve the mount.
///
/// The two processes are joined by a pipe so that the exit code the caller sees
/// is the child's verdict, not a guess made before the child had one. The child
/// sends [`READY`] when it has detached and is about to serve, or the reason it
/// could not, and the first process reports that and leaves with a failing
/// code. Closing the pipe without a word says the child died, which is a
/// failure too. Without this the caller would be told the mount succeeded while
/// the child was still able to fail, and by then its standard error is
/// `/dev/null` and the message is gone.
///
/// The fork happens while this process still has one thread. The FUSE workers
/// start later, inside `Session::run`, so the child gets an address space that
/// no other thread was in the middle of changing. Where `--auto-unmount` is in
/// use, the socket to `fusermount3` is an ordinary descriptor that the child
/// inherits, so the unmount still follows the process that serves the mount.
fn daemonize() -> Result<(), DetachError> {
    let mut fds: [libc::c_int; 2] = [0; 2];
    // SAFETY: `pipe` fills the two element array it is given.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } < 0 {
        return Err(DetachError::Local(std::io::Error::last_os_error()));
    }
    let [read_fd, write_fd] = fds;
    // SAFETY: `fork` has no preconditions.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        let error = std::io::Error::last_os_error();
        // SAFETY: both descriptors are open and neither is used again.
        unsafe {
            libc::close(read_fd);
            libc::close(write_fd);
        };
        return Err(DetachError::Local(error));
    }
    if pid > 0 {
        // SAFETY: the write end belongs to the child from here on.
        unsafe { libc::close(write_fd) };
        await_child(read_fd);
    }
    // SAFETY: the read end belongs to the parent, which is another process.
    unsafe { libc::close(read_fd) };
    let detached = detach();
    match &detached {
        Ok(()) => send(write_fd, &[READY]),
        Err(error) => send(write_fd, error.to_string().as_bytes()),
    }
    // SAFETY: the write end is open and is not used again. Closing it is what
    // tells the parent that nothing more is coming.
    unsafe { libc::close(write_fd) };
    // The message went down the pipe, so the error itself has nowhere left to
    // go: this process no longer has a standard error the caller can see.
    detached.map_err(|_sent| DetachError::Reported)
}

/// Reports what the child had to say and ends the first process.
///
/// This never returns, and it leaves through `_exit`: a normal return would
/// drop the session, and dropping it unmounts what the child is serving.
fn await_child(read_fd: libc::c_int) -> ! {
    let mut message = Vec::new();
    let mut buf = [0u8; 256];
    loop {
        // SAFETY: the pointer and length describe `buf`.
        let read = unsafe { libc::read(read_fd, buf.as_mut_ptr().cast(), buf.len()) };
        let Ok(read) = usize::try_from(read) else {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break;
        };
        if read == 0 {
            break;
        }
        if message.is_empty() && buf[0] == READY {
            // SAFETY: `_exit` ends the process and has no preconditions.
            unsafe { libc::_exit(0) };
        }
        message.extend_from_slice(&buf[..read]);
    }
    if message.is_empty() {
        eprintln!("zipfs: the process serving the mount stopped before it was ready");
    } else {
        eprintln!("zipfs: {}", String::from_utf8_lossy(&message));
    }
    // SAFETY: `_exit` ends the process and has no preconditions.
    unsafe { libc::_exit(1) };
}

/// Sends the parent everything it needs, as far as it gets.
///
/// A short write here costs a diagnostic, so there is nothing to gain by
/// reporting one: the caller learns of the failure from the exit code, which
/// closing the pipe delivers on its own.
fn send(fd: libc::c_int, mut bytes: &[u8]) {
    while !bytes.is_empty() {
        // SAFETY: the pointer and length describe `bytes`.
        let written = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
        let Ok(written) = usize::try_from(written) else {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return;
        };
        if written == 0 {
            return;
        }
        bytes = &bytes[written..];
    }
}

/// Leaves the terminal behind, in the child.
fn detach() -> std::io::Result<()> {
    // SAFETY: `setsid` has no preconditions. It fails only when this process
    // already leads a group, which the child of a fork never does.
    if unsafe { libc::setsid() } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // Holding the caller's directory would keep it busy for the life of the
    // mount. The mount point itself is safe: fuser resolves it before this.
    // SAFETY: the argument is a NUL terminated string that lives for the call.
    if unsafe { libc::chdir(c"/".as_ptr()) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    redirect_standard_streams()
}

/// Points the standard streams at `/dev/null`.
///
/// A daemon that kept them would write over whatever the caller does next, and
/// would hold the terminal open. After the process detaches, logs go to
/// syslog, and `--foreground` keeps them on standard error.
fn redirect_standard_streams() -> std::io::Result<()> {
    // SAFETY: the path is a NUL terminated string that lives for the call.
    let null = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDWR) };
    if null < 0 {
        return Err(std::io::Error::last_os_error());
    }
    for target in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
        // SAFETY: `null` is open and the targets are the standard descriptors.
        if unsafe { libc::dup2(null, target) } < 0 {
            let error = std::io::Error::last_os_error();
            // SAFETY: `null` is open and is not used again.
            unsafe { libc::close(null) };
            return Err(error);
        }
    }
    if null > libc::STDERR_FILENO {
        // SAFETY: `null` is open and every use of it has finished.
        unsafe { libc::close(null) };
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn syslog_sends_each_line_as_a_datagram() {
        let path = std::env::temp_dir().join(format!("zipfs-syslog-{}", std::process::id()));
        let _ = fs::remove_file(&path);
        let daemon = UnixDatagram::bind(&path).unwrap();
        daemon.set_nonblocking(true).unwrap();
        let syslog = Syslog::open(path.clone()).unwrap();

        let log = |level, args| syslog.log(&Record::builder().level(level).args(args).build());
        log(Level::Warn, format_args!("plain"));
        log(Level::Error, format_args!("one\ntw\0o\n\nthree"));

        let mut received = Vec::new();
        let mut buf = [0u8; 1024];
        while let Ok(n) = daemon.recv(&mut buf) {
            received.push(String::from_utf8_lossy(&buf[..n]).into_owned());
        }
        fs::remove_file(&path).unwrap();

        let tail = format!(" zipfs[{}]: ", std::process::id());
        let messages: Vec<_> = received
            .iter()
            .map(|d| (&d[..4], d.split_once(&tail).unwrap().1))
            .collect();
        assert_eq!(
            messages,
            [
                ("<28>", "plain"),
                ("<27>", "one"),
                ("<27>", "two"),
                ("<27>", "three")
            ]
        );
    }
}
