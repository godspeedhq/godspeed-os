// SPDX-License-Identifier: GPL-2.0-only
//! The NTP server this service asks, as a setting: `/system/services/time/ntp`
//! (`docs/developer-services-design.md`, "the same rule for the parameters other services compile in").
//!
//! One IPv4 address, `#` starting a comment, blank lines ignored. An address and not a name, for the reason
//! the constant it replaces was one: a name needs a DNS lookup, which `net-stack` runs inside its loop.
//!
//! Names nothing outside `core`, so `scripts/host_test_check.py` compiles this file on its own and runs its
//! tests on the host. Reading and writing the file is `main.rs`'s.

/// The file, in this service's own folder beside its floor (`docs/filesystem-layout.md`, L2).
pub const PATH: &[u8] = b"/system/services/time/ntp";

/// The server asked when the file says nothing else: time.cloudflare.com, anycast.
pub const DEFAULT: [u8; 4] = [162, 159, 200, 123];

/// What a missing file is written with, so the file always shows the server in effect.
pub const DEFAULT_FILE: &[u8] = b"# The NTP server the `time` service asks: one IPv4 address. Takes effect at the\n\
# next `date sync`, or the next boot.\n\
162.159.200.123  # time.cloudflare.com\n";

/// Why the file was refused. A file that does not say one address is never half used: the network time
/// is not asked for until it is fixed, rather than quietly asking a server the file did not name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bad {
    /// No address in it at all, only comments or blank lines.
    Empty,
    /// A line that is not a dotted IPv4 address, numbered from 1.
    NotAnAddress(usize),
    /// A second address. One server is asked, so which one is meant is not this service's to guess.
    TwoAddresses(usize),
    /// Not text.
    NotText,
}

/// The server the file names.
pub fn parse(file: &[u8]) -> Result<[u8; 4], Bad> {
    let text = core::str::from_utf8(file).map_err(|_| Bad::NotText)?;
    let mut found: Option<[u8; 4]> = None;
    for (i, raw) in text.lines().enumerate() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let ip = ipv4(line).ok_or(Bad::NotAnAddress(i + 1))?;
        if found.is_some() {
            return Err(Bad::TwoAddresses(i + 1));
        }
        found = Some(ip);
    }
    found.ok_or(Bad::Empty)
}

/// `a.b.c.d`, each 0 to 255, decimal, nothing else. An unspecified (0.0.0.0) or broadcast
/// (255.255.255.255) address names no server, so both are refused.
pub fn ipv4(s: &str) -> Option<[u8; 4]> {
    let mut out = [0u8; 4];
    let mut parts = s.split('.');
    for o in out.iter_mut() {
        let p = parts.next()?;
        if p.is_empty() || p.len() > 3 || !p.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let v: u16 = p.parse().ok()?;
        if v > 255 {
            return None;
        }
        *o = v as u8;
    }
    if parts.next().is_some() || out == [0, 0, 0, 0] || out == [255, 255, 255, 255] {
        return None;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_file_names_the_default() {
        assert_eq!(parse(DEFAULT_FILE), Ok(DEFAULT));
    }

    #[test]
    fn an_address_with_comments_around_it() {
        assert_eq!(parse(b"# mine\n\n  192.168.4.1   # the router\n# end\n"), Ok([192, 168, 4, 1]));
        assert_eq!(parse(b"10.0.2.2"), Ok([10, 0, 2, 2]));
        assert_eq!(parse(b"10.0.2.2\r\n"), Ok([10, 0, 2, 2]));
    }

    #[test]
    fn a_file_that_does_not_say_one_address_is_refused() {
        assert_eq!(parse(b""), Err(Bad::Empty));
        assert_eq!(parse(b"# nothing\n\n"), Err(Bad::Empty));
        assert_eq!(parse(b"# a name is not taken\ntime.cloudflare.com\n"), Err(Bad::NotAnAddress(2)));
        assert_eq!(parse(b"1.2.3.4\n5.6.7.8\n"), Err(Bad::TwoAddresses(2)));
        assert_eq!(parse(&[0xff, 0xfe]), Err(Bad::NotText));
    }

    #[test]
    fn only_a_dotted_quad_reads_as_an_address() {
        assert_eq!(ipv4("162.159.200.123"), Some(DEFAULT));
        for bad in ["", "1.2.3", "1.2.3.4.5", "256.1.1.1", "1..2.3", "1.2.3.-4", "+1.2.3.4", "0001.2.3.4",
                    "1.2.3.4 5", "0.0.0.0", "255.255.255.255", "a.b.c.d"] {
            assert_eq!(ipv4(bad), None, "{bad}");
        }
    }
}
