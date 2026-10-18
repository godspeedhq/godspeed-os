// SPDX-License-Identifier: GPL-2.0-only
//! The package manager's configuration (`docs/developer-services-design.md` 10.8): the registries it knows
//! and the one in use, the plain settings, how each value is checked, and how the file is read and
//! rewritten.
//!
//! Names nothing outside `core`, so `scripts/host_test_check.py` compiles this file on its own and runs
//! its tests on the host. Everything that touches the machine is in `main.rs`.
//!
//! # The file
//!
//! ```text
//! registry <name> <https url> <sha256:pin>     one line per registry this machine knows
//! use <name>                                   the one in use: searched, installed from
//! tls-roots /system/services/godspeed/roots/   plain settings, `key value`
//! ```
//!
//! Exactly one registry is in use, so no package is ever found in two places at once and there is no
//! priority to decide. A file written before registries had names - `registry <url>` and `trust-root
//! <pin>` on separate lines - reads as one registry called `default`, and is rewritten in the new form the
//! first time a registry command changes it.

/// The file, in the service's own folder (`docs/filesystem-layout.md`, L2).
pub const DIR: &str = "/system/services/godspeed";
pub const PATH: &str = "/system/services/godspeed/config";
/// The largest config file read. Eight registries and every setting fit with room to spare.
pub const FILE_MAX: usize = 2048;
/// The most registries one machine knows.
pub const MAX_REGISTRIES: usize = 8;
/// The longest registry name.
pub const NAME_MAX: usize = 32;

/// The registry a new machine knows, and is using.
pub const DEFAULT_REGISTRY: Registry<'static> = Registry {
    name: "default",
    url: "https://godspeed-registry.bankole-422.workers.dev",
    pin: "sha256:d769d1a64c3105c8da0ffcc2f54aba1c3a16ce4ae707a6d206556f08824a146d",
};

/// One plain setting: its name, its default, and what it is (shown by `godspeed config`).
pub struct Key {
    pub name: &'static str,
    pub default: &'static str,
    pub about: &'static str,
}

/// Every plain setting, in the order the file and `godspeed config` show them.
pub const KEYS: [Key; 4] = [
    Key {
        name: "tls-roots",
        default: "/system/services/godspeed/roots/",
        about: "the folder of certificate authorities TLS accepts, one DER file each",
    },
    Key { name: "timeout-secs", default: "30", about: "how long one request may take" },
    Key { name: "max-package-mib", default: "64", about: "the largest package that will be downloaded" },
    Key {
        name: "offline-max-age-days",
        default: "30",
        about: "how old a package file's registry check may be before installing it asks (`accept-old`)",
    },
];

/// The folder the default root is written to on first start, and the root itself.
pub const DEFAULT_ROOTS: &str = "/system/services/godspeed/roots/";
pub const DEFAULT_ROOT_FILE: &str = "/system/services/godspeed/roots/isrg-root-x2.der";

/// A registry: what it is called here, where it is, and the root its metadata must be signed by.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Registry<'f> {
    pub name: &'f str,
    pub url: &'f str,
    pub pin: &'f str,
}

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

/// The configuration as read. A file with a problem is never half used: `problem` makes every command
/// refuse until it is fixed.
pub struct Config<'f> {
    pub values: [(&'f str, Source); KEYS.len()],
    regs: [Registry<'f>; MAX_REGISTRIES],
    nregs: usize,
    /// Which of `regs` is in use, and whether a `use` line said so.
    using: usize,
    pub using_source: Source,
    /// The file is in the form written before registries had names.
    pub legacy: bool,
    pub problem: Option<Problem<'f>>,
}

/// What is wrong with the file, and on which line (0 for the file as a whole).
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
    /// A `registry` line that is neither `<name> <url> <pin>` nor the old one-word form.
    BadRegistry,
    /// More than [`MAX_REGISTRIES`].
    TooManyRegistries,
    /// `use` names a registry the file does not have.
    NoSuchRegistry(&'f str),
    /// Several registries and no `use` line: which one is meant is not the program's to guess.
    NoneInUse,
    /// `trust-root` beside a named registry, where the pin belongs on the registry's own line.
    StrayTrustRoot,
    NotText,
}

impl<'f> Config<'f> {
    pub fn get(&self, key: &str) -> &'f str {
        // Every caller names a key from `KEYS`; an unknown one reads as empty, which every check refuses.
        KEYS.iter().position(|k| k.name == key).map(|i| self.values[i].0).unwrap_or("")
    }

    pub fn registries(&self) -> &[Registry<'f>] {
        &self.regs[..self.nregs]
    }

    /// The registry in use.
    pub fn in_use(&self) -> Registry<'f> {
        self.regs[self.using]
    }

    pub fn find(&self, name: &str) -> Option<Registry<'f>> {
        self.registries().iter().copied().find(|r| r.name == name)
    }
}

/// Read `text` (the whole file, or `None` when it does not exist).
pub fn parse(text: Option<&[u8]>) -> Config<'_> {
    let mut c = Config {
        values: core::array::from_fn(|i| (KEYS[i].default, Source::Default)),
        regs: [DEFAULT_REGISTRY; MAX_REGISTRIES],
        nregs: 0,
        using: 0,
        using_source: Source::Default,
        legacy: false,
        problem: None,
    };
    let Some(bytes) = text else {
        c.nregs = 1;
        return c;
    };
    let Ok(text) = core::str::from_utf8(bytes) else {
        c.nregs = 1;
        c.problem = Some(Problem { line: 0, what: Bad::NotText });
        return c;
    };
    let mut use_name: Option<(&str, usize)> = None;
    let mut legacy_url: Option<&str> = None;
    let mut legacy_pin: Option<(&str, usize)> = None;
    for (n, raw) in text.lines().enumerate() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let mut words = line.split_whitespace();
        let key = words.next().unwrap_or("");
        let (w1, w2, w3, w4) = (words.next(), words.next(), words.next(), words.next());
        let problem = |what| Some(Problem { line: n + 1, what });
        match key {
            "registry" => match (w1, w2, w3, w4) {
                (Some(name), Some(url), Some(pin), None) => {
                    let reg = Registry { name, url, pin };
                    if let Err(why) = check_registry(&reg) {
                        c.problem = c.problem.or(problem(Bad::Invalid("registry", why)));
                    } else if c.registries().iter().any(|r| r.name == name) {
                        c.problem = c.problem.or(problem(Bad::Twice(name)));
                    } else if c.nregs == MAX_REGISTRIES {
                        c.problem = c.problem.or(problem(Bad::TooManyRegistries));
                    } else {
                        c.regs[c.nregs] = reg;
                        c.nregs += 1;
                    }
                }
                // The form written before registries had names: one address, its pin on its own line.
                (Some(url), None, None, None) if legacy_url.is_none() => match url_check(url) {
                    Ok(()) => legacy_url = Some(url),
                    Err(why) => c.problem = c.problem.or(problem(Bad::Invalid("registry", why))),
                },
                (None, ..) => c.problem = c.problem.or(problem(Bad::NoValue("registry"))),
                _ => c.problem = c.problem.or(problem(Bad::BadRegistry)),
            },
            "trust-root" => match (w1, w2) {
                (Some(pin), None) if legacy_pin.is_none() => match pin_check(pin) {
                    Ok(()) => legacy_pin = Some((pin, n + 1)),
                    Err(why) => c.problem = c.problem.or(problem(Bad::Invalid("trust-root", why))),
                },
                (Some(_), None) => c.problem = c.problem.or(problem(Bad::Twice("trust-root"))),
                (None, _) => c.problem = c.problem.or(problem(Bad::NoValue("trust-root"))),
                _ => c.problem = c.problem.or(problem(Bad::Invalid("trust-root", "one value"))),
            },
            "use" => match (w1, w2, use_name) {
                (Some(name), None, None) => use_name = Some((name, n + 1)),
                (Some(_), None, Some(_)) => c.problem = c.problem.or(problem(Bad::Twice("use"))),
                (None, ..) => c.problem = c.problem.or(problem(Bad::NoValue("use"))),
                _ => c.problem = c.problem.or(problem(Bad::Invalid("use", "one registry's name"))),
            },
            _ => {
                let Some(i) = KEYS.iter().position(|k| k.name == key) else {
                    c.problem = c.problem.or(problem(Bad::UnknownKey(key)));
                    continue;
                };
                let Some(value) = w1 else {
                    c.problem = c.problem.or(problem(Bad::NoValue(key)));
                    continue;
                };
                if w2.is_some() {
                    c.problem = c.problem.or(problem(Bad::Invalid(key, "one value")));
                } else if c.values[i].1 == Source::File {
                    c.problem = c.problem.or(problem(Bad::Twice(key)));
                } else if let Err(why) = check(key, value) {
                    c.problem = c.problem.or(problem(Bad::Invalid(key, why)));
                } else {
                    c.values[i] = (value, Source::File);
                }
            }
        }
    }
    // The old form is one registry called `default`, unless named ones exist too, which is a mix nobody wrote.
    if let Some(url) = legacy_url {
        if c.nregs > 0 {
            c.problem = c.problem.or(Some(Problem { line: 0, what: Bad::BadRegistry }));
        } else {
            c.regs[0] = Registry { name: "default", url, pin: legacy_pin.map(|(p, _)| p).unwrap_or(DEFAULT_REGISTRY.pin) };
            c.nregs = 1;
            c.legacy = true;
        }
    } else if let Some((_, line)) = legacy_pin {
        c.problem = c.problem.or(Some(Problem { line, what: Bad::StrayTrustRoot }));
    }
    if c.nregs == 0 {
        // A file that names no registry knows the default one, as a new machine does.
        c.nregs = 1;
    }
    match use_name {
        Some((name, line)) => match c.registries().iter().position(|r| r.name == name) {
            Some(i) => {
                c.using = i;
                c.using_source = Source::File;
            }
            None => c.problem = c.problem.or(Some(Problem { line, what: Bad::NoSuchRegistry(name) })),
        },
        None if c.nregs > 1 => c.problem = c.problem.or(Some(Problem { line: 0, what: Bad::NoneInUse })),
        None => {}
    }
    c
}

/// Whether `value` is acceptable for plain setting `key`, and if not, why, in words a person can act on.
pub fn check(key: &str, value: &str) -> Result<(), &'static str> {
    match key {
        "tls-roots" => {
            if value.starts_with('/') && value.ends_with('/') && value.len() > 1 && !value.contains("//") {
                Ok(())
            } else {
                Err("tls-roots is a folder: a full path ending in /")
            }
        }
        "timeout-secs" => number(value, 1, 100, "timeout-secs is a whole number of seconds, 1 to 100"),
        "max-package-mib" => number(value, 1, 4096, "max-package-mib is a whole number, 1 to 4096"),
        "offline-max-age-days" => number(value, 0, 3650, "offline-max-age-days is a whole number of days, 0 to 3650"),
        _ => Err("no such setting"),
    }
}

/// Whether a registry can be added: a name, an `https://` address, a pin.
pub fn check_registry(r: &Registry<'_>) -> Result<(), &'static str> {
    name_check(r.name)?;
    url_check(r.url)?;
    pin_check(r.pin)
}

/// A registry's name: what `use` and `remove` call it. Letters, digits and hyphens.
pub fn name_check(name: &str) -> Result<(), &'static str> {
    let ok = !name.is_empty() && name.len() <= NAME_MAX && !name.starts_with('-')
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if ok { Ok(()) } else { Err("a registry's name is 1 to 32 lower-case letters, digits and hyphens") }
}

fn url_check(url: &str) -> Result<(), &'static str> {
    self::url(url).map(|_| ())
}

/// The registry's signing root: `sha256:` and 64 hex digits.
pub fn pin_check(pin: &str) -> Result<(), &'static str> {
    let ok = pin.strip_prefix("sha256:").is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()));
    if ok { Ok(()) } else { Err("a signing root is sha256: and 64 hex digits") }
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
    let rest = s.strip_prefix("https://").ok_or("a registry is https:// only: the transport is TLS")?;
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

/// The file written on first start: the default registry in use and every setting at its default, with
/// what each is, so the file always shows what is in effect and nothing is a hidden default (26.4).
pub fn defaults_file(out: &mut [u8]) -> usize {
    let mut w = Out { buf: out, n: 0 };
    w.put("# The godspeed package manager's settings. # starts a comment.\n");
    w.put("# Read at the start of every command. `godspeed config` and `godspeed registry` show them.\n");
    w.put("\n");
    registry_block(&mut w, &[DEFAULT_REGISTRY], DEFAULT_REGISTRY.name);
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

/// The registries, and the one in use, as the file holds them.
fn registry_block(w: &mut Out<'_>, regs: &[Registry<'_>], using: &str) {
    w.put("# The registries this machine knows: name, address, the root their metadata is signed by.\n");
    w.put("# Exactly one is in use (`godspeed registry use <name>`).\n");
    for r in regs {
        w.put("registry ");
        w.put(r.name);
        w.put(" ");
        w.put(r.url);
        w.put(" ");
        w.put(r.pin);
        w.put("\n");
    }
    w.put("use ");
    w.put(using);
    w.put("\n");
}

/// The file with plain setting `key` set to `value`: its line replaced where it is, or added at the end,
/// every other line kept exactly as it was. `None` when the result would not fit `out`.
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

/// The file with its registries replaced by `regs` and `using` in use. Every line that is not about
/// registries is kept exactly as it was; the registry lines, the `use` line and an old `trust-root` line
/// are replaced by one block where the first of them stood (or at the end, if there were none), so the
/// comments around them survive. This is also how an old file moves to the new form.
pub fn with_registries(text: &str, regs: &[Registry<'_>], using: &str, out: &mut [u8]) -> Option<usize> {
    let mut w = Out { buf: out, n: 0 };
    let mut placed = false;
    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        let first = line.split(char::is_whitespace).next().unwrap_or("");
        let ours = matches!(first, "registry" | "use" | "trust-root");
        // The block's own two comment lines are written with it, so they are not kept twice.
        let block_comment = raw.starts_with("# The registries this machine knows") || raw.starts_with("# Exactly one is in use");
        if ours || block_comment {
            if !placed {
                registry_block(&mut w, regs, using);
                placed = true;
            }
        } else {
            w.put(raw);
            w.put("\n");
        }
    }
    if !placed {
        registry_block(&mut w, regs, using);
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

    const PIN_B: &str = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn text(out: &[u8], n: usize) -> &str {
        core::str::from_utf8(&out[..n]).unwrap_or("")
    }

    #[test]
    fn no_file_is_the_default_registry_and_every_default() {
        let c = parse(None);
        assert!(c.problem.is_none());
        assert_eq!(c.registries(), &[DEFAULT_REGISTRY]);
        assert_eq!(c.in_use(), DEFAULT_REGISTRY);
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
        assert_eq!(c.in_use(), DEFAULT_REGISTRY);
        assert_eq!(c.using_source, Source::File);
        assert!(!c.legacy);
        for (i, k) in KEYS.iter().enumerate() {
            assert_eq!(c.values[i], (k.default, Source::File));
        }
    }

    #[test]
    fn every_default_passes_its_own_check() {
        for k in KEYS.iter() {
            assert_eq!(check(k.name, k.default), Ok(()), "{}", k.name);
        }
        assert_eq!(check_registry(&DEFAULT_REGISTRY), Ok(()));
    }

    #[test]
    fn several_registries_and_one_in_use() {
        let file = ["registry default https://a.example/ ", DEFAULT_REGISTRY.pin, "\nregistry mirror https://b.example/ ",
                    PIN_B, "\nuse mirror\n"].concat();
        let c = parse(Some(file.as_bytes()));
        assert!(c.problem.is_none(), "{:?}", c.problem);
        assert_eq!(c.registries().len(), 2);
        assert_eq!(c.in_use().name, "mirror");
        assert_eq!(c.in_use().pin, PIN_B);
        assert_eq!(c.find("default").map(|r| r.url), Some("https://a.example/"));
    }

    #[test]
    fn which_registry_is_meant_is_never_guessed() {
        let two = ["registry a https://a.example/ ", PIN_B, "\nregistry b https://b.example/ ", PIN_B, "\n"].concat();
        assert_eq!(parse(Some(two.as_bytes())).problem, Some(Problem { line: 0, what: Bad::NoneInUse }));
        let named = ["registry a https://a.example/ ", PIN_B, "\nuse c\n"].concat();
        assert_eq!(parse(Some(named.as_bytes())).problem, Some(Problem { line: 2, what: Bad::NoSuchRegistry("c") }));
        let dup = ["registry a https://a.example/ ", PIN_B, "\nregistry a https://b.example/ ", PIN_B, "\n"].concat();
        assert_eq!(parse(Some(dup.as_bytes())).problem, Some(Problem { line: 2, what: Bad::Twice("a") }));
        // One registry needs no `use` line.
        let one = ["registry a https://a.example/ ", PIN_B, "\n"].concat();
        let c = parse(Some(one.as_bytes()));
        assert!(c.problem.is_none());
        assert_eq!((c.in_use().name, c.using_source), ("a", Source::Default));
    }

    #[test]
    fn a_file_from_before_named_registries_reads_as_default() {
        let old = ["registry https://old.example/\ntrust-root ", PIN_B, "\ntimeout-secs 9\n"].concat();
        let c = parse(Some(old.as_bytes()));
        assert!(c.problem.is_none(), "{:?}", c.problem);
        assert!(c.legacy);
        assert_eq!(c.in_use(), Registry { name: "default", url: "https://old.example/", pin: PIN_B });
        assert_eq!(c.get("timeout-secs"), "9");
        // Rewritten in the new form, it reads the same and is no longer legacy.
        let mut out = [0u8; FILE_MAX];
        let n = with_registries(&old, c.registries(), "default", &mut out).unwrap_or(0);
        let again = parse(Some(&out[..n]));
        assert!(again.problem.is_none(), "{:?}", again.problem);
        assert!(!again.legacy);
        assert_eq!(again.in_use(), c.in_use());
        assert!(!text(&out, n).contains("trust-root"));
        assert!(text(&out, n).contains("timeout-secs 9"));
        // A pin with no registry beside it is named, not silently ignored.
        let stray = ["registry a https://a.example/ ", PIN_B, "\ntrust-root ", PIN_B, "\n"].concat();
        assert_eq!(parse(Some(stray.as_bytes())).problem, Some(Problem { line: 2, what: Bad::StrayTrustRoot }));
    }

    #[test]
    fn a_file_with_a_problem_says_which_line() {
        let c = parse(Some(b"timeout-secs 5\nspeed 9\n"));
        assert_eq!(c.problem, Some(Problem { line: 2, what: Bad::UnknownKey("speed") }));
        let c = parse(Some(b"# x\ntimeout-secs 0\n"));
        assert!(matches!(c.problem, Some(Problem { line: 2, what: Bad::Invalid("timeout-secs", _) })));
        let c = parse(Some(b"timeout-secs 5\ntimeout-secs 6\n"));
        assert_eq!(c.problem, Some(Problem { line: 2, what: Bad::Twice("timeout-secs") }));
        let c = parse(Some(b"registry\n"));
        assert_eq!(c.problem, Some(Problem { line: 1, what: Bad::NoValue("registry") }));
        let c = parse(Some(b"registry a https://a.example/\n"));
        assert_eq!(c.problem, Some(Problem { line: 1, what: Bad::BadRegistry }));
        let c = parse(Some(b"registry http://a.example/\n"));
        assert!(matches!(c.problem, Some(Problem { line: 1, what: Bad::Invalid("registry", _) })));
        let c = parse(Some(&[0xff, 0xfe]));
        assert_eq!(c.problem, Some(Problem { line: 0, what: Bad::NotText }));
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
    fn registry_names_and_pins() {
        for good in ["default", "mirror-2", "a"] {
            assert_eq!(name_check(good), Ok(()), "{good}");
        }
        for bad in ["", "-x", "Upper", "has space", "dot.ted", &"a".repeat(33)] {
            assert!(name_check(bad).is_err(), "{bad}");
        }
        assert!(pin_check("sha256:abc").is_err());
        assert!(pin_check(PIN_B).is_ok());
    }

    #[test]
    fn rewriting_the_registries_keeps_every_other_line() {
        let mut buf = [0u8; FILE_MAX];
        let n = defaults_file(&mut buf);
        let file = text(&buf, n);
        let regs = [DEFAULT_REGISTRY, Registry { name: "mirror", url: "https://b.example/", pin: PIN_B }];
        let mut out = [0u8; FILE_MAX];
        let m = with_registries(file, &regs, "mirror", &mut out).unwrap_or(0);
        let c = parse(Some(&out[..m]));
        assert!(c.problem.is_none(), "{:?}", c.problem);
        assert_eq!(c.registries(), &regs);
        assert_eq!(c.in_use().name, "mirror");
        let new = text(&out, m);
        assert!(new.contains("# how long one request may take\ntimeout-secs 30"));
        assert_eq!(new.matches("# The registries this machine knows").count(), 1);
        // Removing one keeps the rest.
        let k = with_registries(new, &regs[..1], "default", &mut buf).unwrap_or(0);
        let c = parse(Some(&buf[..k]));
        assert_eq!(c.registries(), &regs[..1]);
    }

    #[test]
    fn setting_a_value_keeps_every_other_line() {
        let text = "# keep me\ntimeout-secs 30  # note\n";
        let mut out = [0u8; 256];
        let n = with_value(text, "timeout-secs", "45", &mut out).unwrap_or(0);
        assert_eq!(core::str::from_utf8(&out[..n]).unwrap_or(""), "# keep me\ntimeout-secs 45\n");
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
        assert!(check("tls-roots", "/system/roots").is_err());
        assert!(check("tls-roots", "/system/roots/").is_ok());
        assert!(check("max-package-mib", "4097").is_err());
        assert!(check("registry", "https://a.example/").is_err(), "a registry is not a plain setting");
    }
}
