//! The settings, and the two ways of naming one.
//!
//! `mount -t fuse.zipfs archive.zip /mnt` reaches this program through
//! `/sbin/mount.fuse`, which execs `zipfs '<archive>' '<mountpoint>' -o <opts>`.
//! Every setting therefore has to be reachable through `-o` as well as through
//! its own long option. [`SETTINGS`] is the one place a setting is written
//! down: both spellings and the help text come from it, so a setting cannot
//! reach one of them and miss the others.

use log::{debug, warn};
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::str::FromStr;
use std::time::Duration;
use zipfs::Config;

/// What a setting does with what it was given.
pub enum Apply {
    /// A setting that is named on its own.
    Flag(fn(&mut Config)),
    /// A setting that is named with a value.
    Value(fn(&mut Config, &str) -> Result<(), String>),
}

/// One thing about a mount that the caller can decide.
pub struct Setting {
    /// The name, which `--name` and `-o name` both spell.
    pub name: &'static str,
    /// Another spelling that an option list may use.
    pub alias: Option<&'static str>,
    /// What the value is called in the help, or `None` for a flag.
    pub metavar: Option<&'static str>,
    /// What it does, a line at a time.
    pub help: &'static [&'static str],
    /// How to put it into the configuration.
    pub apply: Apply,
}

/// Every setting, in the order the help lists them.
pub const SETTINGS: &[Setting] = &[
    Setting {
        name: "threads",
        alias: None,
        metavar: Some("N"),
        help: &["FUSE worker threads [default: cores, up to 8]"],
        apply: Apply::Value(|c, v| {
            c.threads = number(v)?;
            Ok(())
        }),
    },
    Setting {
        name: "no-verify",
        alias: Some("noverify"),
        metavar: None,
        help: &[
            "skip the CRC32 check on entries that are read",
            "to the end",
        ],
        apply: Apply::Flag(|c| c.verify = false),
    },
    Setting {
        name: "verify",
        alias: None,
        metavar: None,
        help: &["check entries against their CRC32 [default]"],
        apply: Apply::Flag(|c| c.verify = true),
    },
    Setting {
        name: "source-buffer-size",
        alias: None,
        metavar: Some("N"),
        help: &["compressed bytes each decoder buffers", "[default: 524288]"],
        apply: Apply::Value(|c, v| {
            c.source_buffer = number(v)?;
            Ok(())
        }),
    },
    Setting {
        name: "decoders-per-file",
        alias: None,
        metavar: Some("N"),
        help: &["idle decoders one open file may keep [default: 4]"],
        apply: Apply::Value(|c, v| {
            c.decoders_per_file = number(v)?;
            Ok(())
        }),
    },
    Setting {
        name: "max-retained-decoders",
        alias: None,
        metavar: Some("N"),
        help: &["idle decoders the mount may keep [default: 64]"],
        apply: Apply::Value(|c, v| {
            c.max_retained_decoders = number(v)?;
            Ok(())
        }),
    },
    Setting {
        name: "uid",
        alias: None,
        metavar: Some("ID"),
        help: &["owner reported for every file [default: caller]"],
        apply: Apply::Value(|c, v| {
            c.uid = number(v)?;
            Ok(())
        }),
    },
    Setting {
        name: "gid",
        alias: None,
        metavar: Some("ID"),
        help: &["group reported for every file [default: caller]"],
        apply: Apply::Value(|c, v| {
            c.gid = number(v)?;
            Ok(())
        }),
    },
    Setting {
        name: "file-mode",
        alias: None,
        metavar: Some("MODE"),
        help: &[
            "permissions for files whose entry records none",
            "[default: 644]",
        ],
        apply: Apply::Value(|c, v| {
            c.file_mode = mode(v)?;
            Ok(())
        }),
    },
    Setting {
        name: "dir-mode",
        alias: None,
        metavar: Some("MODE"),
        help: &[
            "permissions for directories whose entry records",
            "none [default: 755]",
        ],
        apply: Apply::Value(|c, v| {
            c.dir_mode = mode(v)?;
            Ok(())
        }),
    },
    // The mask settings say which bits to take away, which is how the
    // filesystems that mount(8) users know best spell this.
    Setting {
        name: "umask",
        alias: None,
        metavar: Some("MASK"),
        help: &["bits to take away from both modes above"],
        apply: Apply::Value(|c, v| {
            let mask = mode(v)?;
            c.file_mode = 0o666 & !mask;
            c.dir_mode = 0o777 & !mask;
            Ok(())
        }),
    },
    Setting {
        name: "fmask",
        alias: None,
        metavar: Some("MASK"),
        help: &["bits to take away from the file mode"],
        apply: Apply::Value(|c, v| {
            c.file_mode = 0o666 & !mode(v)?;
            Ok(())
        }),
    },
    Setting {
        name: "dmask",
        alias: None,
        metavar: Some("MASK"),
        help: &["bits to take away from the directory mode"],
        apply: Apply::Value(|c, v| {
            c.dir_mode = 0o777 & !mode(v)?;
            Ok(())
        }),
    },
    Setting {
        name: "attr-ttl",
        alias: None,
        metavar: Some("SECS"),
        help: &[
            "how long the kernel may cache metadata",
            "[default: 31536000]",
        ],
        apply: Apply::Value(|c, v| {
            c.attr_ttl = Duration::from_secs(number(v)?);
            Ok(())
        }),
    },
    Setting {
        name: "allow-other",
        alias: None,
        metavar: None,
        help: &["let other users see the mount"],
        apply: Apply::Flag(|c| c.allow_other = true),
    },
    Setting {
        name: "allow-root",
        alias: None,
        metavar: None,
        help: &["let root see the mount"],
        apply: Apply::Flag(|c| c.allow_root = true),
    },
    Setting {
        name: "auto-unmount",
        alias: None,
        metavar: None,
        help: &[
            "unmount if this process dies; needs",
            "--allow-other or --allow-root",
        ],
        apply: Apply::Flag(|c| c.auto_unmount = true),
    },
];

/// The column the help text starts in.
const HELP_COLUMN: usize = 32;

/// Finds the setting of this name, in either spelling.
pub fn find(name: &str) -> Option<&'static Setting> {
    SETTINGS.iter().find(|setting| {
        same_name(setting.name, name) || setting.alias.is_some_and(|a| same_name(a, name))
    })
}

/// Whether two names are the same, counting `-` and `_` as one character.
///
/// Option lists are written with underscores and long options with dashes, and
/// a setting answers to both.
fn same_name(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes().zip(b.bytes()).all(|(x, y)| {
            x == y || (matches!(x, b'-' | b'_') && matches!(y, b'-' | b'_'))
        })
}

/// Applies a setting that a long option named, taking its value if it needs one.
///
/// # Errors
///
/// Returns an error when the value is missing or does not parse.
pub fn apply_long(
    setting: &Setting,
    config: &mut Config,
    parser: &mut lexopt::Parser,
) -> Result<(), lexopt::Error> {
    match setting.apply {
        Apply::Flag(set) => set(config),
        Apply::Value(set) => {
            let raw = parser.value()?;
            let name = setting.name;
            let text = raw
                .to_str()
                .ok_or_else(|| format!("the value for '--{name}' is not valid text"))?;
            set(config, text).map_err(|reason| format!("option '--{name}': {reason}"))?;
        }
    }
    Ok(())
}

/// Renders the settings as the help lists them.
pub fn settings_help() -> String {
    let mut out = String::new();
    for setting in SETTINGS {
        let named = match setting.metavar {
            Some(metavar) => format!("      --{} {metavar}", setting.name),
            None => format!("      --{}", setting.name),
        };
        for (i, line) in setting.help.iter().enumerate() {
            let start = if i == 0 { named.as_str() } else { "" };
            let _ = writeln!(out, "{start:<HELP_COLUMN$}{line}");
        }
    }
    out
}

/// Options that the kernel, `mount(8)` or `fusermount3` acts on by itself.
///
/// The mount gets these whether or not this program looks at them, so accepting
/// them without comment is what lets an fstab line work.
const IGNORED: &[&str] = &[
    "ro",
    "atime",
    "noatime",
    "relatime",
    "norelatime",
    "strictatime",
    "nostrictatime",
    "diratime",
    "nodiratime",
    "dev",
    "nodev",
    "suid",
    "nosuid",
    "exec",
    "noexec",
    "sync",
    "async",
    "dirsync",
    "auto",
    "noauto",
    "defaults",
    "user",
    "users",
    "nouser",
    "owner",
    "group",
    "_netdev",
    "nofail",
    "silent",
    "loud",
    "seclabel",
    "default_permissions",
];

/// Options that are ignored for the same reason and that carry a value.
///
/// These are matched by name, after the value has been taken off.
const IGNORED_WITH_VALUE: &[&str] = &[
    "subtype",
    "fsname",
    "comment",
    "context",
    "fscontext",
    "defcontext",
    "rootcontext",
];

/// The one family of options that is known by how a name starts: everything
/// mount(8) keeps for itself and its callers.
const IGNORED_PREFIX: &str = "x-";

/// Applies one `-o` list to the configuration.
///
/// # Errors
///
/// Returns a message for an option that is understood but cannot be honoured,
/// or whose value does not parse. An option that is not recognised is reported
/// through the log and does not stop the mount, because a mount option this
/// program has never heard of is normally one the kernel deals with.
pub fn apply(config: &mut Config, spec: &OsStr) -> Result<(), String> {
    let text = spec
        .to_str()
        .ok_or_else(|| String::from("option list is not valid text"))?;
    // A value with a comma in it, such as an SELinux context, cannot be told
    // apart from two options here. Those all belong to the kernel, and the
    // pieces land among the ignored prefixes.
    let opts: Vec<&str> = text.split(',').filter(|o| !o.is_empty()).collect();
    // What 'rw' means depends on whether it asks for a new mount or asks an
    // existing one to change, and either word can come first, so this is
    // settled before anything is applied.
    if opts.contains(&"remount") && opts.contains(&"rw") {
        return Err("the mount is read-only, so 'remount,rw' cannot be honoured".into());
    }
    for opt in opts {
        apply_one(config, opt)?;
    }
    Ok(())
}

fn apply_one(config: &mut Config, opt: &str) -> Result<(), String> {
    let (key, value) = match opt.split_once('=') {
        Some((key, value)) => (key, Some(value)),
        None => (opt, None),
    };
    if let Some(setting) = find(key) {
        return match setting.apply {
            Apply::Flag(set) => {
                if value.is_some() {
                    return Err(format!("mount option '{key}' takes no value"));
                }
                set(config);
                Ok(())
            }
            Apply::Value(set) => {
                let value =
                    value.ok_or_else(|| format!("mount option '{key}' needs a value"))?;
                set(config, value)
                    .map_err(|reason| format!("mount option '{key}={value}': {reason}"))
            }
        };
    }
    // What is left belongs to the mount rather than to this program.
    match key {
        "rw" => {
            // mount(8) puts 'rw' in the list whenever read-only was not asked
            // for, so it arrives with a plain `mount -t fuse.zipfs` and with
            // any fstab line that leaves it out. The mount is read-only
            // whatever the list says, which the kernel reports back, so this
            // is nothing to stop for and too common to warn about.
            debug!("the mount is read-only, so 'rw' has no effect");
        }
        "remount" => {
            if value.is_some() {
                return Err(String::from("mount option 'remount' takes no value"));
            }
            // zipfs cannot change a mount that already exists. Mounting again
            // is all that can follow, which is worth saying out loud.
            warn!("a remount cannot change an existing mount, so a new mount follows");
        }
        _ => {
            if !ignored(key) {
                warn!("ignoring unknown mount option '{opt}'");
            }
        }
    }
    Ok(())
}

fn ignored(key: &str) -> bool {
    IGNORED.contains(&key) || IGNORED_WITH_VALUE.contains(&key) || key.starts_with(IGNORED_PREFIX)
}

/// Reads a number, reporting only why it could not.
///
/// The caller knows which option this was and how it was spelled, so the
/// message says nothing about that.
fn number<T>(value: &str) -> Result<T, String>
where
    T: FromStr,
    T::Err: std::fmt::Display,
{
    value.parse().map_err(|e| format!("expected a number: {e}"))
}

fn mode(value: &str) -> Result<u16, String> {
    u16::from_str_radix(value.trim_start_matches("0o"), 8)
        .map(|m| m & 0o7777)
        .map_err(|e| format!("expected an octal mode such as 644: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn parse(spec: &str) -> Result<Config, String> {
        let mut config = Config::default();
        apply(&mut config, &OsString::from(spec))?;
        Ok(config)
    }

    #[test]
    fn every_setting_is_reachable_both_ways_and_is_in_the_help() {
        let help = settings_help();
        for setting in SETTINGS {
            let dashed = setting.name;
            let underscored = dashed.replace('-', "_");
            assert!(find(dashed).is_some(), "'{dashed}' is not found by name");
            assert!(
                find(&underscored).is_some(),
                "'{underscored}' is not found by name"
            );
            assert!(
                help.contains(dashed),
                "'{dashed}' is missing from the help"
            );
        }
    }

    #[test]
    fn a_long_option_and_an_option_list_set_the_same_thing() {
        let mut from_list = Config::default();
        apply(
            &mut from_list,
            &OsString::from("threads=3,attr-ttl=60,no-verify,umask=077"),
        )
        .unwrap();

        let mut from_long = Config::default();
        let mut parser = lexopt::Parser::from_args([
            "--threads",
            "3",
            "--attr-ttl",
            "60",
            "--no-verify",
            "--umask",
            "077",
        ]);
        while let Some(arg) = parser.next().unwrap() {
            let lexopt::Arg::Long(name) = arg else {
                panic!("only long options are given here");
            };
            let setting = find(name).expect("every name here is a setting");
            apply_long(setting, &mut from_long, &mut parser).unwrap();
        }

        assert_eq!(from_list.threads, from_long.threads);
        assert_eq!(from_list.attr_ttl, from_long.attr_ttl);
        assert_eq!(from_list.verify, from_long.verify);
        assert_eq!(from_list.file_mode, from_long.file_mode);
        assert_eq!(from_list.dir_mode, from_long.dir_mode);
    }

    #[test]
    fn a_list_sets_every_option_it_names() {
        let config = parse("threads=3,attr-ttl=60,no-verify,allow_other").unwrap();
        assert_eq!(config.threads, 3);
        assert_eq!(config.attr_ttl, Duration::from_secs(60));
        assert!(!config.verify);
        assert!(config.allow_other);
    }

    #[test]
    fn underscores_and_dashes_name_the_same_option() {
        assert_eq!(parse("source_buffer_size=8192").unwrap().source_buffer, 8192);
        assert_eq!(parse("source-buffer-size=8192").unwrap().source_buffer, 8192);
    }

    #[test]
    fn what_mount_adds_on_its_own_is_accepted() {
        // The list mount(8) hands a helper for a plain fstab line.
        let config =
            parse("rw,nosuid,nodev,noatime,subtype=zipfs,fsname=/a.zip,x-gvfs-hide").unwrap();
        assert!(config.verify);
    }

    #[test]
    fn the_options_mount_adds_are_known_by_name() {
        // The value is off by the time a name is looked up, which is what
        // makes this worth stating.
        for opt in [
            "subtype=zipfs",
            "fsname=/a.zip",
            "comment=made by hand",
            "rootcontext=system_u:object_r:tmp_t",
            "x-gvfs-hide",
            "ro",
            "nosuid",
        ] {
            let key = opt.split_once('=').map_or(opt, |(key, _)| key);
            assert!(ignored(key), "'{opt}' should be left to the kernel");
        }
    }

    #[test]
    fn an_unknown_option_is_left_to_the_kernel() {
        parse("something_new").unwrap();
    }

    #[test]
    fn the_rw_that_mount_supplies_is_accepted() {
        // This is the whole option list for a plain `mount -t fuse.zipfs`.
        parse("rw").unwrap();
    }

    #[test]
    fn asking_an_existing_mount_to_become_writable_is_refused() {
        parse("remount,rw").unwrap_err();
        parse("rw,remount").unwrap_err();
    }

    #[test]
    fn a_value_that_does_not_parse_is_refused() {
        parse("threads=many").unwrap_err();
        parse("file-mode=9999").unwrap_err();
        parse("threads").unwrap_err();
        parse("allow_other=1").unwrap_err();
    }

    #[test]
    fn a_mask_takes_bits_away() {
        let config = parse("umask=022").unwrap();
        assert_eq!(config.file_mode, 0o644);
        assert_eq!(config.dir_mode, 0o755);
        assert_eq!(parse("dmask=077").unwrap().dir_mode, 0o700);
    }
}
