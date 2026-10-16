// SPDX-License-Identifier: GPL-2.0-only
//! The package manager's configuration (`docs/developer-services-design.md` 10.8): what each key is, its
//! default, how a value is checked, and how the file is read and rewritten.
//!
//! Names nothing outside `core`, so `scripts/host_test_check.py` compiles this file on its own and runs
//! its tests on the host. Everything that touches the machine is in `main.rs`.

/// The file, in the service's own folder (`docs/filesystem-layout.md`, L2).
pub const DIR: &str = "/system/services/godspeed";
pub const PATH: &str = "/system/services/godspeed/config";
/// The largest config file read. One line per key and a few comments fit many times over.
pub const FILE_MAX: usize = 2048;

/// One key: its name, its default, and what it is (shown by `godspeed config`).
pub struct Key {
    pub name: &'static str,
    pub default: &'static str,
    pub about: &'static str,
}

/// Every key, in the order the file and `godspeed config` show them.
pub const KEYS: [Key; 5] = [
    Key {
        name: "registry",
        default: "https://godspeed-registry.bankole-422.workers.dev",
        about: "where packages are read; https only",
    },
    Key {
        name: "trust-root",
        default: "sha256:d769d1a64c3105c8da0ffcc2f54aba1c3a16ce4ae707a6d206556f08824a146d",
        about: "the registry's signing root, pinned (not used until metadata is verified)",
    },
    Key {
        name: "tls-roots",
        default: "/system/services/godspeed/roots/",
        about: "the folder of certificate authorities TLS accepts, one DER file each",
    },
    Key { name: "timeout-secs", default: "30", about: "how long one request may take" },
    Key { name: "max-package-mib", default: "64", about: "the largest package that will be downloaded" },
];

/// The folder the default root is written to on first start, and the root itself.
pub const DEFAULT_ROOTS: &str = "/system/services/godspeed/roots/";
pub const DEFAULT_ROOT_FILE: &str = "/system/services/godspeed/roots/isrg-root-x2.der";

/// Where a value came from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    File,
    Default,
}

impl Source {
    pub fn word(self) -> &'static str {
        match self {
            Source::File => "file",
            Source::Default => "default",
        }
    }
}

/// The configuration as read: each key's value and where it came from, plus the first problem in the file,
/// if any. A file with a problem is never half-used: `problem` makes every command refuse until it is fixed.
pub struct Config<'f> {
    pub values: [(&'f str, Source); 5],
    pub problem: Option<Problem<'f>>,
}

/// What is wrong with the file, and on which line.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Problem<'f> {
    pub line: usize,
    pub what: Bad<'f>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Bad<'f> {
    UnknownKey(&'f str),
    NoValue(&'f str),
    Twice(&'f str),
    Invalid(&'f str, &'static str),
    NotText,
}

impl Config<'_> {
    pub fn get(&self, key: &str) -> &str {
        // Every caller names a key from `KEYS`; an unknown one reads as empty, which every check refuses.
        KEYS.iter().position(|k| k.name == key).map(|i| self.values[i].0).unwrap_or("")
    }
}

/// Read `text` (the whole file, or `None` when it does not exist).
pub fn parse(text: Option<&[u8]>) -> Config<'_> {
    let mut c = Config { values: core::array::from_fn(|i| (KEYS[i].default, Source::Default)), problem: None };
    let Some(bytes) = text else { return c };
    let Ok(text) = core::str::from_utf8(bytes) else {
        c.problem = Some(Problem { line: 0, what: Bad::NotText });
        return c;
    };
    for (n, raw) in text.lines().enumerate() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let (key, value) = match line.split_once(char::is_whitespace) {
            Some((k, v)) => (k, v.trim()),
            None => (line, ""),
        };
        let problem = |what| Some(Problem { line: n + 1, what });
        let Some(i) = KEYS.iter().position(|k| k.name == key) else {
            c.problem = c.problem.or(problem(Bad::UnknownKey(key)));
            continue;
        };
        if value.is_empty() {
            c.problem = c.problem.or(problem(Bad::NoValue(key)));
            continue;
        }
        if c.values[i].1 == Source::File {
            c.problem = c.problem.or(problem(Bad::Twice(key)));
            continue;
        }
        if let Err(why) = check(key, value) {
            c.problem = c.problem.or(problem(Bad::Invalid(key, why)));
            continue;
        }
        c.values[i] = (value, Source::File);
    }
    c
}

/// Whether `value` is acceptable for `key`, and if not, why, in words a person can act on.
pub fn check(key: &str, value: &str) -> Result<(), &'static str> {
    match key {
        "registry" => url(value).map(|_| ()),
        "trust-root" => {
            let hex = value.strip_prefix("sha256:").ok_or("a trust root is sha256: and 64 hex digits")?;
            if hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                Ok(())
            } else {
                Err("a trust root is sha256: and 64 hex digits")
            }
        }
        "tls-roots" => {
            if value.starts_with('/') && value.ends_with('/') && value.len() > 1 && !value.contains("//") {
                Ok(())
            } else {
                Err("tls-roots is a folder: a full path ending in /")
            }
        }
        "timeout-secs" => number(value, 1, 100, "timeout-secs is a whole number of seconds, 1 to 100"),
        "max-package-mib" => number(value, 1, 4096, "max-package-mib is a whole number, 1 to 4096"),
        _ => Err("no such key"),
    }
}

fn number(value: &str, lo: u32, hi: u32, why: &'static str) -> Result<(), &'static str> {
    match value.parse::<u32>() {
        Ok(n) if (lo..=hi).contains(&n) => Ok(()),
        _ => Err(why),
    }
}

/// A number already checked by [`check`].
pub fn number_of(value: &str) -> u32 {
    value.parse().unwrap_or(0)
}

/// A registry address, taken apart.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Url<'u> {
    pub host: &'u str,
    pub port: u16,
    /// The path to the registry's root, ending in `/` (the registry's files are read relative to it).
    pub base: &'u str,
}

/// `https://host[:port][/path]`. Anything else is refused: the transport is TLS (10.3), so an `http://`
/// address is a mistake to be named, never one to fall back to.
pub fn url(s: &str) -> Result<Url<'_>, &'static str> {
    let rest = s.strip_prefix("https://").ok_or("the registry is https:// only: the transport is TLS")?;
    let (authority, base) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if !base.ends_with('/') {
        return Err("the registry's path must end in /");
    }
    if base.bytes().any(|b| b <= b' ' || b == b'?' || b == b'#' || b >= 0x7f) {
        return Err("the registry's path has a character a URL path cannot");
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().ok().filter(|&p| p != 0).ok_or("the port is a number, 1 to 65535")?),
        None => (authority, 443),
    };
    if !is_dns_name(host) {
        return Err("the registry's host must be a DNS name, which TLS checks the certificate against");
    }
    Ok(Url { host, port, base })
}

/// Letters, digits and hyphens in dot-separated labels, and not an address: TLS needs a name to check.
fn is_dns_name(h: &str) -> bool {
    let b = h.as_bytes();
    if b.is_empty() || b.len() > 253 {
        return false;
    }
    let labels_ok = b.split(|&c| c == b'.').all(|l| {
        !l.is_empty() && l.len() <= 63 && l[0] != b'-' && l[l.len() - 1] != b'-'
            && l.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'-')
    });
    let last = b.rsplit(|&c| c == b'.').next().unwrap_or(b);
    labels_ok && !last.iter().all(u8::is_ascii_digit)
}

/// Epoch seconds as a UTC calendar date and time: (year, month, day, hour, minute). The days-to-civil
/// conversion is the standard one for the proleptic Gregorian calendar (Hinnant, "chrono-Compatible
/// Low-Level Date Algorithms").
pub fn civil(epoch: u64) -> (u64, u64, u64, u64, u64) {
    let days = epoch / 86_400;
    let secs = epoch % 86_400;
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    (year, month, day, secs / 3600, secs % 3600 / 60)
}

/// The file written on first start: every key at its default, with what each is, so the file always
/// shows what is in effect and nothing is a hidden default (26.4).
pub fn defaults_file(out: &mut [u8]) -> usize {
    let mut w = Out { buf: out, n: 0 };
    w.put("# The godspeed package manager's settings. One `key value` per line; # starts a comment.\n");
    w.put("# Read at the start of every command. `godspeed config` shows each value and where it came from.\n");
    for k in KEYS.iter() {
        w.put("\n# ");
        w.put(k.about);
        w.put("\n");
        w.put(k.name);
        w.put(" ");
        w.put(k.default);
        w.put("\n");
    }
    w.n
}

/// The file with `key` set to `value`: its line replaced where it is, or added at the end, every other
/// line kept exactly as it was. `None` when the result would not fit `out`.
pub fn with_value(text: &str, key: &str, value: &str, out: &mut [u8]) -> Option<usize> {
    let mut w = Out { buf: out, n: 0 };
    let mut done = false;
    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        let this = line.split(char::is_whitespace).next() == Some(key);
        if this && !done {
            w.put(key);
            w.put(" ");
            w.put(value);
            w.put("\n");
            done = true;
        } else if !this {
            w.put(raw);
            w.put("\n");
        }
    }
    if !done {
        w.put(key);
        w.put(" ");
        w.put(value);
        w.put("\n");
    }
    (!w.overflow()).then_some(w.n)
}

struct Out<'b> {
    buf: &'b mut [u8],
    n: usize,
}

impl Out<'_> {
    fn put(&mut self, s: &str) {
        let b = s.as_bytes();
        // Once past the end nothing more is copied, but `n` keeps counting so `overflow` can say so.
        if self.n < self.buf.len() {
            let end = (self.n + b.len()).min(self.buf.len());
            self.buf[self.n..end].copy_from_slice(&b[..end - self.n]);
        }
        self.n += b.len();
    }
    fn overflow(&self) -> bool {
        self.n > self.buf.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_file_is_every_default() {
        let c = parse(None);
        assert!(c.problem.is_none());
        for (i, k) in KEYS.iter().enumerate() {
            assert_eq!(c.values[i], (k.default, Source::Default));
        }
    }

    #[test]
    fn the_defaults_file_reads_back_as_every_default_from_the_file() {
        let mut buf = [0u8; FILE_MAX];
        let n = defaults_file(&mut buf);
        let c = parse(Some(&buf[..n]));
        assert!(c.problem.is_none(), "{:?}", c.problem);
        for (i, k) in KEYS.iter().enumerate() {
            assert_eq!(c.values[i], (k.default, Source::File));
        }
    }

    #[test]
    fn every_default_passes_its_own_check() {
        for k in KEYS.iter() {
            assert_eq!(check(k.name, k.default), Ok(()), "{}", k.name);
        }
    }

    #[test]
    fn a_file_with_a_problem_says_which_line() {
        let c = parse(Some(b"registry https://a.example/\nspeed 9\n"));
        assert_eq!(c.problem, Some(Problem { line: 2, what: Bad::UnknownKey("speed") }));
        let c = parse(Some(b"# x\ntimeout-secs 0\n"));
        assert!(matches!(c.problem, Some(Problem { line: 2, what: Bad::Invalid("timeout-secs", _) })));
        let c = parse(Some(b"timeout-secs 5\ntimeout-secs 6\n"));
        assert_eq!(c.problem, Some(Problem { line: 2, what: Bad::Twice("timeout-secs") }));
        let c = parse(Some(b"registry\n"));
        assert_eq!(c.problem, Some(Problem { line: 1, what: Bad::NoValue("registry") }));
        let c = parse(Some(&[0xff, 0xfe]));
        assert_eq!(c.problem, Some(Problem { line: 0, what: Bad::NotText }));
    }

    #[test]
    fn a_value_from_the_file_wins_and_comments_are_ignored() {
        let c = parse(Some(b"registry https://registry.example:8443/pkgs/   # moved\n"));
        assert!(c.problem.is_none());
        assert_eq!(c.get("registry"), "https://registry.example:8443/pkgs/");
        assert_eq!(c.values[0].1, Source::File);
        assert_eq!(c.get("timeout-secs"), "30");
    }

    #[test]
    fn registry_addresses() {
        assert_eq!(url("https://a.example"), Ok(Url { host: "a.example", port: 443, base: "/" }));
        assert_eq!(url("https://a.example:8443/x/"), Ok(Url { host: "a.example", port: 8443, base: "/x/" }));
        for bad in ["http://a.example", "https://", "https://10.0.0.1", "https://a.example:0",
                    "https://a.example:99999", "https://a.example/x", "https://a b/", "https://a.example/x?y/",
                    "ftp://a.example"] {
            assert!(url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn setting_a_value_keeps_every_other_line() {
        let text = "# keep me\nregistry https://old.example\ntimeout-secs 30  # note\n";
        let mut out = [0u8; 256];
        let n = with_value(text, "timeout-secs", "45", &mut out).unwrap_or(0);
        assert_eq!(core::str::from_utf8(&out[..n]).unwrap_or(""), "# keep me\nregistry https://old.example\ntimeout-secs 45\n");
        let n = with_value(text, "max-package-mib", "8", &mut out).unwrap_or(0);
        assert!(core::str::from_utf8(&out[..n]).unwrap_or("").ends_with("timeout-secs 30  # note\nmax-package-mib 8\n"));
        let mut tiny = [0u8; 8];
        assert_eq!(with_value(text, "timeout-secs", "45", &mut tiny), None);
    }

    #[test]
    fn epoch_seconds_as_a_date() {
        assert_eq!(civil(0), (1970, 1, 1, 0, 0));
        assert_eq!(civil(1_792_159_859), (2026, 10, 16, 14, 10));
        assert_eq!(civil(951_782_400), (2000, 2, 29, 0, 0));
        assert_eq!(civil(4_102_444_799), (2099, 12, 31, 23, 59));
    }

    #[test]
    fn checks_name_what_is_wrong() {
        assert!(check("trust-root", "sha256:abc").is_err());
        assert!(check("trust-root", &["sha256:", &"a".repeat(64)].concat()).is_ok());
        assert!(check("tls-roots", "/system/roots").is_err());
        assert!(check("tls-roots", "/system/roots/").is_ok());
        assert!(check("max-package-mib", "4097").is_err());
        assert!(check("nope", "1").is_err());
    }
}
