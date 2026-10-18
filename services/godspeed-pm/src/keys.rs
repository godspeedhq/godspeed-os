// SPDX-License-Identifier: GPL-2.0-only
//! The keys this machine's OPERATOR trusts directly (`docs/developer-services-design.md` 15.6): `godspeed trust add
//! <publisher> <key>`, `godspeed trust remove`, and their card in `godspeed trust`.
//!
//! A registry vouches for a publisher's key through its signed metadata; a key added here is vouched for by the
//! person at this machine and nobody else. It is what lets a package built with no network - `godspeed package --gsp`
//! - be installed with none. What it does not have, said wherever it shows: nothing tells this machine if the key
//! leaks or is withdrawn. Only removing it here does, and the supervisor's check at every start reads this list
//! again (`check.rs`, `operator_trusts`), so a key removed stops its packages at their next start.
//!
//! One line a key, `<publisher> <key>`, in `check::OPERATOR_KEYS`, which is on `/system` and so exactly as
//! protected as every other trust record is (12).

use core::fmt::Write as _;
use godspeed::{self as gs, ServiceContext};
use godspeed_package::{PackageName, PublicKey};

use crate::check::{OPERATOR_KEYS, OPERATOR_KEYS_MAX};
use crate::{cell, Cell, Rows};

/// The most keys the list holds.
const KEYS_MAX: usize = 32;

/// A publisher name as packages use it, and a key as signatures name it: refused by name if either is not one.
fn parse<'a>(publisher: &'a str, key: &str) -> Result<(&'a str, Cell), &'static str> {
    let mut probe = Cell::new();
    let _ = write!(probe, "{}/x", publisher);
    if publisher.contains('/') || PackageName::parse(probe.as_str()).is_err() {
        return Err("a publisher is the part before the `/` of a package's name, as in godspeed");
    }
    let k: PublicKey = key.parse().map_err(|_| "a key is ed25519: and 64 hex digits, as `godspeed key show` prints it")?;
    Ok((publisher, cell(format_args!("{}", k))))
}

/// The list as it stands: its text, and how long.
fn read(ctx: &ServiceContext, buf: &mut [u8; OPERATOR_KEYS_MAX]) -> Result<usize, gs::Error> {
    match gs::fs::Fs::new(ctx).read_into(OPERATOR_KEYS, buf) {
        Ok(n) => Ok(n),
        Err(gs::Error::NotFound) => Ok(0),
        Err(e) => Err(e),
    }
}

/// Every `(publisher, key)` line of `text`.
fn entries(text: &str) -> impl Iterator<Item = (&str, &str)> {
    text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).filter_map(|l| {
        let mut w = l.split_whitespace();
        Some((w.next()?, w.next()?))
    })
}

/// Write the list: a heading, then every line `keep` lets through, then `add` if given.
fn write(ctx: &ServiceContext, text: &str, keep: &dyn Fn(&str, &str) -> bool, add: Option<(&str, &str)>) -> Result<(), &'static str> {
    let mut out = [0u8; OPERATOR_KEYS_MAX];
    let mut n = 0usize;
    let mut put = |s: &str| -> bool {
        if n + s.len() > out.len() { return false; }
        out[n..n + s.len()].copy_from_slice(s.as_bytes());
        n += s.len();
        true
    };
    let mut ok = put("# keys this machine's operator trusts directly, for unpublished package files: <publisher> <key>\n");
    for (p, k) in entries(text) {
        if keep(p, k) { ok &= put(p) && put(" ") && put(k) && put("\n"); }
    }
    if let Some((p, k)) = add { ok &= put(p) && put(" ") && put(k) && put("\n"); }
    if !ok { return Err("the list of trusted keys would be larger than this machine keeps"); }
    gs::fs::Fs::new(ctx).write(OPERATOR_KEYS, &out[..n]).map_err(|_| "the list of trusted keys could not be written")
}

/// `godspeed trust add <publisher> <key>`. The shell has asked first, naming what it means.
pub fn add(ctx: &ServiceContext, r: &mut Rows, publisher: &str, key: &str) {
    let (publisher, key) = match parse(publisher, key) { Ok(x) => x, Err(why) => return r.fail("trust", key, why) };
    let mut buf = [0u8; OPERATOR_KEYS_MAX];
    let n = match read(ctx, &mut buf) { Ok(n) => n, Err(e) => return r.fail("trust", OPERATOR_KEYS, cell(format_args!("could not be read: {:?}", e)).as_str()) };
    let text = core::str::from_utf8(&buf[..n]).unwrap_or("");
    if entries(text).any(|(p, k)| p == publisher && k == key.as_str()) {
        r.columns(&["key", "value", "note"]);
        r.row("title", publisher, "already trusted by this machine's operator");
        r.row("key", key.as_str(), "nothing changed");
        return;
    }
    if entries(text).count() >= KEYS_MAX { return r.fail("trust", OPERATOR_KEYS, "it holds as many keys as it may: remove one first"); }
    if let Err(why) = write(ctx, text, &|_, _| true, Some((publisher, key.as_str()))) { return r.fail("trust", OPERATOR_KEYS, why); }
    r.columns(&["key", "value", "note"]);
    r.row("title", publisher, "trusted by this machine's operator");
    r.row("key", key.as_str(), "a package file signed by it, for this publisher, installs with no registry");
    r.note(format_args!("nothing tells this machine if that key leaks: `godspeed trust remove {} {}` ends it", publisher, key.as_str()));
}

/// `godspeed trust remove <publisher> <key>`.
pub fn remove(ctx: &ServiceContext, r: &mut Rows, publisher: &str, key: &str) {
    let (publisher, key) = match parse(publisher, key) { Ok(x) => x, Err(why) => return r.fail("trust", key, why) };
    let mut buf = [0u8; OPERATOR_KEYS_MAX];
    let n = match read(ctx, &mut buf) { Ok(n) => n, Err(e) => return r.fail("trust", OPERATOR_KEYS, cell(format_args!("could not be read: {:?}", e)).as_str()) };
    let text = core::str::from_utf8(&buf[..n]).unwrap_or("");
    if !entries(text).any(|(p, k)| p == publisher && k == key.as_str()) {
        return r.fail("trust", key.as_str(), "this machine's operator does not trust that key for that publisher");
    }
    let gone = key.as_str();
    if let Err(why) = write(ctx, text, &|p, k| !(p == publisher && k == gone), None) { return r.fail("trust", OPERATOR_KEYS, why); }
    r.columns(&["key", "value", "note"]);
    r.row("title", publisher, "no longer trusted for this key");
    r.row("key", gone, "a package it signed does not start again: the supervisor checks this list at every start");
}

/// The operator's card in `godspeed trust`, after the registries'.
pub fn show(ctx: &ServiceContext, r: &mut Rows) {
    let mut buf = [0u8; OPERATOR_KEYS_MAX];
    let n = read(ctx, &mut buf).unwrap_or(0);
    let text = core::str::from_utf8(&buf[..n]).unwrap_or("");
    r.row("title", "operator", "keys trusted directly here, for unpublished package files");
    let mut any = false;
    for (p, k) in entries(text) {
        any = true;
        r.row(cell(format_args!("key {}/", p)).as_str(), k, "trusted by the operator; no registry vouches for it");
    }
    if !any { r.row("keys", "none", "`godspeed trust add <publisher> <key>` trusts one"); }
}
