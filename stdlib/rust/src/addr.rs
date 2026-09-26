// SPDX-License-Identifier: Apache-2.0
//! Addresses. Pure parsing - no service and no SDK. (There is no formatter here; this said "parsing and formatting".)
//!
//! Separate from [`crate::net`] for one reason: this is the only part of the network surface a
//! caller feeds UNTRUSTED TEXT to, it is the classic place to be off by one, and it has no
//! dependencies - so it can be unit-tested on the host, where a wrong answer is a failing test
//! rather than a machine quietly talking to the wrong address.
//!
//! That split is the pattern `kernel/src/clock.rs` documents: pure logic lives where it can be
//! tested, and the part that talks to a service is exercised on the target.

/// An IPv4 address.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Ipv4(pub [u8; 4]);

impl Ipv4 {
    /// Parse dotted-quad. Returns `None` rather than guessing at anything malformed.
    pub fn parse(s: &str) -> Option<Ipv4> {
        let mut out = [0u8; 4];
        let mut parts = 0usize;
        for field in s.split('.') {
            if parts == 4 || field.is_empty() || field.len() > 3 {
                return None;
            }
            let mut v: u16 = 0;
            for c in field.bytes() {
                if !c.is_ascii_digit() {
                    return None;
                }
                v = v * 10 + (c - b'0') as u16;
            }
            if v > 255 {
                return None;
            }
            out[parts] = v as u8;
            parts += 1;
        }
        if parts == 4 { Some(Ipv4(out)) } else { None }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ordinary_addresses() {
        assert_eq!(Ipv4::parse("10.0.2.2"), Some(Ipv4([10, 0, 2, 2])));
        assert_eq!(Ipv4::parse("0.0.0.0"), Some(Ipv4([0, 0, 0, 0])));
        assert_eq!(Ipv4::parse("255.255.255.255"), Some(Ipv4([255, 255, 255, 255])));
        assert_eq!(Ipv4::parse("192.168.4.43"), Some(Ipv4([192, 168, 4, 43])));
    }

    /// The cases that matter, because each one is an address a careless parser would ACCEPT and
    /// then send traffic to. A wrong answer here is not a crash, it is the machine talking to
    /// somebody else.
    #[test]
    fn refuses_what_is_not_an_address() {
        assert_eq!(Ipv4::parse("256.0.0.1"), None, "an octet past 255");
        assert_eq!(Ipv4::parse("1.2.3"), None, "three octets");
        assert_eq!(Ipv4::parse("1.2.3.4.5"), None, "five octets");
        assert_eq!(Ipv4::parse(""), None);
        assert_eq!(Ipv4::parse("..."), None, "four empty fields");
        assert_eq!(Ipv4::parse("1.2.3."), None, "a trailing dot leaves an empty field");
        assert_eq!(Ipv4::parse(".1.2.3"), None, "and so does a leading one");
        assert_eq!(Ipv4::parse("1.2.3.x"), None, "a non-digit");
        assert_eq!(Ipv4::parse("1.2.3.-1"), None, "a sign is not a digit");
        assert_eq!(Ipv4::parse("1.2.3.4 "), None, "trailing space is not trimmed for us");
        assert_eq!(Ipv4::parse("1.2.3.0004"), None, "more digits than an octet can hold");
    }

    /// `0999` is under 255 only if you let it overflow into four digits. The length guard is what
    /// stops that, and this pins it, because removing the guard still passes every test above.
    #[test]
    fn octet_length_is_bounded_before_the_value_is() {
        assert_eq!(Ipv4::parse("1.2.3.0255"), None);
        assert_eq!(Ipv4::parse("1.2.3.255"), Some(Ipv4([1, 2, 3, 255])));
    }
}
