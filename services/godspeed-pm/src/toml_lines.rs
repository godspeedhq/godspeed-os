// SPDX-License-Identifier: GPL-2.0-only
//! The TOML a registry serves for a package, read line by line without a heap.
//!
//! The package index and the manifest are TOML that the developer CLI writes, and the shared crate reads
//! them only with a heap (its `toml` feature). This is the native side's answer to that crate's open
//! question 1, "how the native side reads the text without a heap" (`docs/developer-services-design.md` 10.6):
//! the subset those files use, and nothing more. Comments, blank lines, `[table]`, `[[array]]`, and
//! `key = "string"`, `key = integer`, `key = true|false` or `key = ["string", ...]`, each on one line. Anything else - an
//! array of anything but strings, an array over several lines, an inline table, a float, a multi-line string - is refused
//! with its line number rather than guessed at. (The array is a contract's: `ipc_send = ["fs"]`.)
//!
//! **A string is returned as written, between its quotes, and must not need unescaping.** A backslash is
//! reported as [`Val::Escaped`] so the caller decides: a field that must match exactly (a name, a version, a
//! digest) refuses it, and a description shown to a person may show it as written. Borrowing the text this
//! way is what keeps the reader heapless.
//!
//! Pure `core`, tested on the host (`scripts/host_test_check.py`).

/// One meaningful line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Line<'a> {
    /// `[name]`
    Table(&'a str),
    /// `[[name]]`
    ArrayTable(&'a str),
    /// `key = value`
    Pair(&'a str, Val<'a>),
}

/// A value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Val<'a> {
    /// A basic string with no backslash in it, between its quotes.
    Str(&'a str),
    /// A basic string that has a backslash escape in it, as written between its quotes.
    Escaped(&'a str),
    /// A non-negative integer.
    Int(u64),
    /// `true` or `false` (a contract's capabilities are booleans).
    Bool(bool),
    /// An array of strings with no backslash in any, as written between its brackets; [`strings`] walks it.
    /// Checked when the line is read, so walking it cannot fail.
    Strings(&'a str),
}

/// The strings of a [`Val::Strings`], in order.
pub fn strings(inner: &str) -> impl Iterator<Item = &str> {
    let mut rest = inner;
    core::iter::from_fn(move || {
        let s = rest.trim_start().strip_prefix(',').unwrap_or(rest).trim_start().strip_prefix('"')?;
        let end = s.find('"')?;
        rest = &s[end + 1..];
        Some(&s[..end])
    })
}

/// `"a", "b"` (the inside of an array), checked: strings only, comma-separated, a trailing comma allowed.
fn array_ok(inner: &str) -> bool {
    let mut rest = inner.trim();
    while !rest.is_empty() {
        let Some(s) = rest.strip_prefix('"') else { return false };
        let Some(end) = s.find('"') else { return false };
        let item = &s[..end];
        if item.bytes().any(|b| b == b'\\' || (b < 0x20 && b != b'\t')) { return false; }
        rest = s[end + 1..].trim_start();
        match rest.strip_prefix(',') {
            Some(r) => rest = r.trim_start(),
            None if rest.is_empty() => {}
            None => return false,
        }
    }
    true
}

/// The lines of `text`, numbered from 1, each one `Ok(line)` or `Err(why)`.
pub fn lines(text: &str) -> impl Iterator<Item = (usize, Result<Line<'_>, &'static str>)> {
    text.split('\n').enumerate().filter_map(|(i, raw)| {
        let l = raw.strip_suffix('\r').unwrap_or(raw).trim();
        if l.is_empty() || l.starts_with('#') {
            return None;
        }
        Some((i + 1, line(l)))
    })
}

fn bare_key(k: &str) -> bool {
    !k.is_empty() && k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.')
}

/// What may follow a value: spaces, then nothing or a comment.
fn tail_ok(rest: &str) -> bool {
    let r = rest.trim_start();
    r.is_empty() || r.starts_with('#')
}

fn line(l: &str) -> Result<Line<'_>, &'static str> {
    if let Some(inner) = l.strip_prefix("[[") {
        let (name, rest) = inner.split_once("]]").ok_or("an array table header is not closed")?;
        if !bare_key(name.trim()) || !tail_ok(rest) {
            return Err("an array table header is not `[[name]]`");
        }
        return Ok(Line::ArrayTable(name.trim()));
    }
    if let Some(inner) = l.strip_prefix('[') {
        let (name, rest) = inner.split_once(']').ok_or("a table header is not closed")?;
        if !bare_key(name.trim()) || !tail_ok(rest) {
            return Err("a table header is not `[name]`");
        }
        return Ok(Line::Table(name.trim()));
    }
    let (key, value) = l.split_once('=').ok_or("a line is not `key = value`")?;
    let key = key.trim();
    if !bare_key(key) {
        return Err("a key is not a bare key");
    }
    let value = value.trim_start();
    if let Some(a) = value.strip_prefix('[') {
        let (inner, rest) = a.split_once(']').ok_or("an array is not closed on its line")?;
        if !tail_ok(rest) { return Err("something follows an array on its line"); }
        if !array_ok(inner) { return Err("an array is not strings, each between quotes, separated by commas"); }
        return Ok(Line::Pair(key, Val::Strings(inner)));
    }
    if let Some(s) = value.strip_prefix('"') {
        // The closing quote is the first one not escaped by a backslash.
        let b = s.as_bytes();
        let mut i = 0usize;
        let mut escaped = false;
        while i < b.len() {
            match b[i] {
                b'\\' => {
                    escaped = true;
                    i += 2;
                    continue;
                }
                b'"' => break,
                c if c < 0x20 && c != b'\t' => return Err("a string holds a control character"),
                _ => {}
            }
            i += 1;
        }
        if i >= b.len() {
            return Err("a string is not closed on its line");
        }
        if !tail_ok(&s[i + 1..]) {
            return Err("something follows a string on its line");
        }
        let inner = &s[..i];
        return Ok(Line::Pair(key, if escaped { Val::Escaped(inner) } else { Val::Str(inner) }));
    }
    let end = value.find(|c: char| c.is_whitespace() || c == '#').unwrap_or(value.len());
    let (num, rest) = value.split_at(end);
    if (num == "true" || num == "false") && tail_ok(rest) {
        return Ok(Line::Pair(key, Val::Bool(num == "true")));
    }
    if !tail_ok(rest) || num.is_empty() || !num.bytes().all(|b| b.is_ascii_digit()) || (num.len() > 1 && num.starts_with('0')) {
        return Err("a value is not a string or a non-negative integer");
    }
    let n = num.parse::<u64>().map_err(|_| "an integer is too large")?;
    Ok(Line::Pair(key, Val::Int(n)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = "format = 1\n\n[package]\nname = \"godspeed/hello\"\ndescription = \"The service template from `godspeed new service`, published as an example\"\n\n[[artifact]]\narch = \"x86_64\"\nsize = 18952  # bytes\n";

    #[test]
    fn reads_a_manifest_shape() {
        let got: [(usize, Result<Line<'_>, &str>); 7] = {
            let mut it = lines(MANIFEST);
            core::array::from_fn(|_| it.next().unwrap_or((0, Err("missing"))))
        };
        assert_eq!(got[0], (1, Ok(Line::Pair("format", Val::Int(1)))));
        assert_eq!(got[1], (3, Ok(Line::Table("package"))));
        assert_eq!(got[2].1, Ok(Line::Pair("name", Val::Str("godspeed/hello"))));
        assert!(matches!(got[3].1, Ok(Line::Pair("description", Val::Str(_)))));
        assert_eq!(got[4].1, Ok(Line::ArrayTable("artifact")));
        assert_eq!(got[5].1, Ok(Line::Pair("arch", Val::Str("x86_64"))));
        assert_eq!(got[6].1, Ok(Line::Pair("size", Val::Int(18952))));
    }

    #[test]
    fn escapes_are_reported_not_unescaped() {
        let mut it = lines("d = \"a \\\"quoted\\\" word\"\n");
        assert_eq!(it.next().map(|x| x.1), Some(Ok(Line::Pair("d", Val::Escaped("a \\\"quoted\\\" word")))));
    }

    #[test]
    fn refuses_what_it_does_not_read() {
        for bad in ["k = [1, 2]", "k = [\"a\" \"b\"]", "k = [\"a\"", "k = [\"a\\b\"]", "k = { a = 1 }", "k = 1.5", "k = \"open", "k = \"x\" y", "[t", "k = 01", "= 3", "k = -1",
                    "k = \"\u{1}\""] {
            let mut it = lines(bad);
            assert!(it.next().is_some_and(|x| x.1.is_err()), "{}", bad);
        }
    }

    #[test]
    fn arrays_of_strings() {
        let mut it = lines("ipc_send = [\"fs\", \"events\",]  # peers\nnone = []\n");
        let first = it.next().map(|x| x.1);
        assert_eq!(first, Some(Ok(Line::Pair("ipc_send", Val::Strings("\"fs\", \"events\",")))));
        let inner = match first { Some(Ok(Line::Pair(_, Val::Strings(s)))) => s, _ => "" };
        let got: [Option<&str>; 3] = { let mut s = strings(inner); [s.next(), s.next(), s.next()] };
        assert_eq!(got, [Some("fs"), Some("events"), None]);
        assert_eq!(it.next().map(|x| x.1), Some(Ok(Line::Pair("none", Val::Strings("")))));
        assert_eq!(strings("").next(), None);
    }

    #[test]
    fn crlf_and_comments() {
        let mut b = lines("log_write = true   # a note\nx = false\n");
        assert_eq!(b.next().map(|x| x.1), Some(Ok(Line::Pair("log_write", Val::Bool(true)))));
        assert_eq!(b.next().map(|x| x.1), Some(Ok(Line::Pair("x", Val::Bool(false)))));
        let mut it = lines("# head\r\nk = \"v\" # tail\r\n\r\n");
        assert_eq!(it.next(), Some((2, Ok(Line::Pair("k", Val::Str("v"))))));
        assert_eq!(it.next(), None);
    }
}
