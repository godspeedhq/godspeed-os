// SPDX-License-Identifier: GPL-2.0-only
//! Packages: search, inspect, fetch, list, verify and remove (`docs/developer-services-design.md` 10.1).
//!
//! **Fetched is not installed.** A fetched package is verified and kept under `/system/packages/` (10.5): its
//! signed manifest, its signature, its service contract and this machine's binary, each checked against what
//! the manifest names. This machine cannot start a program from disk yet (the supervisor runs only the images
//! built into it; 9.2), so nothing here runs one, and every answer says so rather than calling it installed.
//!
//! **Every answer is verified first.** Search reads the verified snapshot; info reads an index the snapshot
//! names by digest, and the chosen release's manifest, signature and contract; fetch reads all of that and
//! the binary (`chain.rs`, `godspeed_package`). A file that does not match is never kept. `verify` checks a
//! fetched copy again from what is on disk, without the network.
//!
//! **A command's rows are written after its last request**, so when the server closes a kept-alive
//! connection part way (`Fail::Closed`) the command runs again on a fresh one without saying anything twice.

use core::fmt::Write as _;

use godspeed::{self as gs, ServiceContext};
use godspeed_package::{trust::KeyStatus, Arch, ArtifactVerifier, ManifestSignature, PackageName, PublicKey, Release, Version};

use crate::chain::{self, Docs, Path, Why};
use crate::config;
use crate::https::{self, Fail, Session};
use crate::meta::{self, Artifacts, Releases};
use crate::toml_lines::{lines, Line, Val};
use crate::{cell, Cell, Roots, Rows, C};

/// Where fetched packages live: one folder per publisher, package and version (10.5).
pub const DIR: &str = "/system/packages/";
// Sized to what each holds, as `chain.rs`'s are: `godspeed/hello`'s manifest is 821 bytes and its contract
// 439, and an index grows by about 200 bytes a release.
const INDEX_MAX: usize = 8 * 1024;
const MANIFEST_MAX: usize = 4 * 1024;
const CONTRACT_MAX: usize = 4 * 1024;
// The sizes of the documents a fetched copy keeps are `check.rs`'s, which reads them back.
use crate::check::{MSIG_MAX, ORIGIN_MAX};
/// How many times a command is tried on a fresh connection when the server closed the last one part way.
const ATTEMPTS: u32 = 3;
/// The most fetched versions one listing reads. More are said, not dropped in silence.
pub const MAX_FETCHED: usize = 32;

/// The GodspeedOS release this machine runs, as the workspace says (`build.rs`).
pub const OS_RELEASE: &str = env!("GODSPEED_OS_RELEASE");

/// This machine's architecture, as package manifests name it: the one whose name begins the target triple
/// this program was built for (`build.rs`).
pub fn this_arch() -> Option<Arch> {
    Arch::ALL.into_iter().find(|a| env!("GODSPEED_PM_TARGET").starts_with(a.as_str()))
}

/// Everything a registry command needs before it connects.
pub struct Ready<'c> {
    pub reg: config::Registry<'c>,
    pub url: config::Url<'c>,
    pub now: u64,
    pub timeout: u32,
    pub max_bytes: u64,
}

/// `publisher/name` and an optional `@version`.
fn spec(s: &str) -> Result<(PackageName<'_>, Option<Version>), &'static str> {
    let (n, v) = match s.split_once('@') {
        Some((n, v)) => (n, Some(v)),
        None => (s, None),
    };
    let name = PackageName::parse(n).map_err(|_| "a package is publisher/name, as in godspeed/hello")?;
    let version = match v {
        Some(v) => Some(v.parse::<Version>().map_err(|_| "a version is MAJOR.MINOR.PATCH, as in godspeed/hello@0.1.0")?),
        None => None,
    };
    Ok((name, version))
}

/// Run `f` on a session to the registry `ready` names, again on a fresh one when the server closed the last
/// part way, and turn what went wrong into a row.
fn on_registry(ctx: &ServiceContext, r: &mut Rows, ready: &Ready<'_>, roots: &Roots,
               mut f: impl FnMut(&mut Session<'_, '_, '_, '_>, &mut Rows) -> Result<(), Why>) {
    let mut list: [&[u8]; crate::MAX_ROOTS] = [&[]; crate::MAX_ROOTS];
    for i in 0..roots.count { list[i] = &roots.store[i][..roots.lens[i]]; }
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        let result = https::session(ctx, ready.url.host, ready.url.port, &list[..roots.count], ready.now, ready.timeout,
                                    |s| f(s, r));
        let why = match result {
            Ok(Ok(())) => return,
            Ok(Err(why)) => why,
            Err(fail) => Why::Net(fail),
        };
        if matches!(why, Why::Net(Fail::Closed)) && attempt < ATTEMPTS {
            continue;
        }
        let (item, text) = why_text(&why, ready);
        return r.fail(item, ready.url.host, text.as_str());
    }
}

fn why_text(why: &Why, ready: &Ready<'_>) -> (&'static str, Cell) {
    match why {
        Why::Net(f) => crate::failure(f, config::DEFAULT_ROOTS, "the clock"),
        Why::Missing { path, status } => ("registry", cell(format_args!("{} answered HTTP {}", path.as_str(), status))),
        Why::TooLarge(what) => ("registry", cell(format_args!("{} is larger than this machine reads", what))),
        Why::NotText(what) => ("registry", cell(format_args!("{} is not text", what))),
        Why::Trust { what, err } => ("trust", cell(format_args!("{}: {} - nothing was kept", what, err))),
        Why::PinMismatch => ("trust", cell(format_args!(
            "the registry's root 1 is not the root {} pins ({}) - nothing from it is trusted", ready.reg.name, ready.reg.pin))),
        Why::PinChanged => ("trust", cell(format_args!(
            "the pin for {} changed since this machine first reached it: `godspeed trust reset {}` to trust the new one",
            ready.reg.name, ready.reg.name))),
        Why::Store(e) => ("trust", cell(format_args!("this machine's record of the registry: {:?}", e))),
    }
}

/// The chain's versions, for a row.
fn chain_row(r: &mut Rows, v: &chain::Verified<'_>) {
    r.row("verified", cell(format_args!("root {}, timestamp {}, snapshot {}, namespaces {}",
        v.root.header.version, v.timestamp.header.version, v.snapshot.version, v.namespaces.header.version)).as_str(),
        if v.first_contact { "first contact: root 1 matched the pin" } else { "against the root this machine already trusts" });
}

/// The chain, as the status note the shell prints under the answer: what was verified, in order, and whether
/// this was first contact. `prefix` comes first (a count), `tail` names what was checked after the chain.
fn chain_note(r: &mut Rows, v: &chain::Verified<'_>, prefix: &str) {
    chain_note_through(r, v, prefix, "");
}

fn chain_note_through(r: &mut Rows, v: &chain::Verified<'_>, prefix: &str, tail: &str) {
    // `kept` when the snapshot the timestamp names is the one already on this machine's disk: not downloaded again.
    r.note(format_args!("{}verified: root {} -> timestamp {} -> snapshot {}{} -> namespaces {}{} ({})", prefix,
        v.root.header.version, v.timestamp.header.version, v.snapshot.version, if v.snapshot.fetched { "" } else { " (kept)" },
        v.namespaces.header.version, tail,
        if v.first_contact { "first contact: root 1 matched the pin" } else { "against the root this machine already trusts" }));
}

/// A key as people read it: `ed25519:` and the first and last eight hex digits. `godspeed trust` shows keys whole.
fn short_key(k: &PublicKey) -> Cell {
    let full = cell(format_args!("{}", k));
    let t = full.as_str();
    if t.len() > 30 { cell(format_args!("{}...{}", &t[..16], &t[t.len() - 6..])) } else { full }
}

/// At most `n` characters of `s`, on a character boundary, with `...` when it was cut.
fn clip(s: &str, n: usize) -> Cell {
    if s.chars().count() <= n { return cell(format_args!("{}", s)); }
    let end = s.char_indices().nth(n.saturating_sub(3)).map(|(i, _)| i).unwrap_or(s.len());
    cell(format_args!("{}...", &s[..end]))
}

/// One page of a listing: the packages it describes (latest version, description), two requests each, and an
/// answer holds 64 rows. The next page is `from` the end of this one, and the note says the command.
pub const PAGE: usize = 40;

/// `godspeed search <word>`: the packages the verified snapshot lists whose name holds `word`; with no word
/// (`godspeed list available`), every one. A page of [`PAGE`] at a time, starting `from` that many matches in.
/// Columns `package latest description`. The names come from the snapshot kept on disk, read a line at a time,
/// so the registry's size is the disk's business, not the stack's; the latest version and the description are
/// each listed package's own index and manifest, by the digests the verified snapshot and index name; `fetch`
/// also checks the publisher's signature. The count, the next page and the chain are the status note.
#[inline(never)]
pub fn search(ctx: &ServiceContext, r: &mut Rows, ready: &Ready<'_>, roots: &Roots, word: Option<&str>, from: usize) {
    let mut docs = Docs::new();
    let base = ready.url.base;
    on_registry(ctx, r, ready, roots, |s, r| {
        let mut iso = [0u8; 20];
        let v = chain::verify(ctx, s, base, &ready.reg, chain::iso(ready.now, &mut iso), &mut docs)?;
        // This page's names and index digests, from the disk; every match counted, for the note.
        let mut names = [[0u8; 128]; PAGE];
        let mut lens = [0usize; PAGE];
        let mut digests = [godspeed_package::Sha256Digest([0; 32]); PAGE];
        let (mut np, mut total) = (0usize, 0usize);
        chain::each_index(ctx, &v.snapshot, &mut |name, d| {
            if word.is_some_and(|w| !name.contains(w)) { return true; }
            if total >= from && np < PAGE && name.len() <= 128 {
                names[np][..name.len()].copy_from_slice(name.as_bytes());
                lens[np] = name.len();
                digests[np] = *d;
                np += 1;
            }
            total += 1;
            true
        })?;
        // Every request first, then the rows (a retried command must not say anything twice).
        let mut found: [(Option<Version>, Cell); PAGE] = core::array::from_fn(|_| (None, Cell::new()));
        let mut ibuf = [0u8; INDEX_MAX];
        let mut mbuf = [0u8; MANIFEST_MAX];
        for k in 0..np {
            let name = core::str::from_utf8(&names[k][..lens[k]]).unwrap_or("");
            let (mut latest, mut desc) = (None, Cell::new());
            if let Ok(pn) = PackageName::parse(name) {
                if let Some(text) = index_by_digest(s, base, &pn, &digests[k], &mut ibuf)? {
                    let mut rels = Releases::new();
                    if let Ok(idx) = meta::index(text, &mut rels) {
                        if let Some(rel) = idx.releases.iter().max_by_key(|rel| rel.version).copied() {
                            latest = Some(rel.version);
                            let mut p = registry_dir(&pn, rel.version);
                            p.push("manifest.toml");
                            let ml = chain::required(s, base, p.as_str(), &mut mbuf, "a manifest")?;
                            if godspeed_package::Sha256Digest::of(&mbuf[..ml]) == rel.manifest {
                                let mut arts = Artifacts::new();
                                if let Ok(m) = meta::manifest(chain::text(&mbuf[..ml], "a manifest")?, &mut arts) {
                                    desc = clip(m.manifest.description.unwrap_or(""), 60);
                                }
                            } else {
                                desc = cell(format_args!("(its manifest is not the one its index names)"));
                            }
                        }
                    }
                }
            }
            found[k] = (latest, desc);
        }
        r.columns(&["package", "latest", "description"]);
        for k in 0..np {
            let name = core::str::from_utf8(&names[k][..lens[k]]).unwrap_or("");
            let (latest, desc) = &found[k];
            let lt = match latest { Some(l) => cell(format_args!("{}", l)), None => cell(format_args!("-")) };
            r.entry(&[C::S(name), C::S(lt.as_str()), C::S(desc.as_str())]);
        }
        let next = from + np;
        let count = if total == 0 {
            cell(format_args!("0 packages  -  "))
        } else if np == 0 {
            cell(format_args!("{} package{}, none from {}  -  ", total, if total == 1 { "" } else { "s" }, from))
        } else if next < total || from > 0 {
            let more = if next < total {
                let cmd = match word { Some(w) => cell(format_args!("godspeed search {} from {}", w, next)), None => cell(format_args!("godspeed list available from {}", next)) };
                cell(format_args!("; next: `{}`", cmd.as_str()))
            } else { cell(format_args!("")) };
            cell(format_args!("{} packages, {} to {} here{}  -  ", total, from + 1, next, more.as_str()))
        } else {
            cell(format_args!("{} package{}  -  ", total, if total == 1 { "" } else { "s" }))
        };
        chain_note(r, &v, count.as_str());
        Ok(())
    });
}

/// The package's index, verified against the snapshot kept on disk, or `None` if the registry has never published it.
fn index_text<'b>(ctx: &ServiceContext, s: &mut Session<'_, '_, '_, '_>, base: &str, v: &chain::Verified<'_>,
                  name: &PackageName<'_>, buf: &'b mut [u8; INDEX_MAX]) -> Result<Option<&'b str>, Why> {
    let full = cell(format_args!("{}", name));
    match chain::index_of(ctx, v, full.as_str())? {
        None => Ok(None),
        Some(d) => index_by_digest(s, base, name, &d, buf),
    }
}

/// The package's index, which must be exactly the bytes `listed` names.
fn index_by_digest<'b>(s: &mut Session<'_, '_, '_, '_>, base: &str, name: &PackageName<'_>,
                       listed: &godspeed_package::Sha256Digest, buf: &'b mut [u8; INDEX_MAX]) -> Result<Option<&'b str>, Why> {
    let mut rel = Path::new();
    rel.push("index/").push(name.publisher()).push("/").push(name.name()).push(".toml");
    let len = chain::required(s, base, rel.as_str(), buf, "the package index")?;
    let text = chain::text(&buf[..len], "the package index")?;
    godspeed_package::trust::verify_index_digest(listed, text)
        .map_err(|err| Why::Trust { what: "the package index", err })?;
    Ok(Some(text))
}

/// The release `wanted`, or the latest. `None` after a row naming the versions that are published.
fn release_of<'i>(idx: &godspeed_package::PackageIndex<'i>, wanted: Option<Version>, r: &mut Rows) -> Option<Release<'i>> {
    match wanted {
        Some(want) => match idx.releases.iter().find(|rel| rel.version == want) {
            Some(rel) => Some(*rel),
            None => {
                let mut all = Cell::new();
                for (i, rel) in idx.releases.iter().enumerate() {
                    let _ = write!(all, "{}{}", if i > 0 { ", " } else { "" }, rel.version);
                }
                r.fail("version", cell(format_args!("{}", want)).as_str(), cell(format_args!("not published; these are: {}", all.as_str())).as_str());
                None
            }
        },
        None => idx.releases.iter().max_by_key(|rel| rel.version).copied(),
    }
}

/// One release's documents, as fetched: the manifest, its signature, the contract.
struct Bufs {
    m: [u8; MANIFEST_MAX],
    s: [u8; MSIG_MAX],
    c: [u8; CONTRACT_MAX],
}

impl Bufs {
    fn new() -> Bufs { Bufs { m: [0; MANIFEST_MAX], s: [0; MSIG_MAX], c: [0; CONTRACT_MAX] } }
}

/// What [`documents`] checked: the lengths in [`Bufs`], and who signed.
#[derive(Clone, Copy)]
struct Checked {
    ml: usize,
    sl: usize,
    cl: usize,
    key: PublicKey,
}

/// `packages/<publisher>/<name>/<version>/` on the registry.
fn registry_dir(name: &PackageName<'_>, version: Version) -> Path {
    let mut pkg = Path::new();
    pkg.push("packages/").push(name.publisher()).push("/").push(name.name()).push("/");
    let vtext = cell(format_args!("{}", version));
    pkg.push(vtext.as_str()).push("/");
    pkg
}

/// A release's manifest by the digest the index names, its signature by a key the namespaces vouch for at
/// the release's publication time, and its contract by the manifest's digest, agreeing with the manifest.
/// `Ok(None)` after a row saying which check failed.
fn documents(s: &mut Session<'_, '_, '_, '_>, base: &str, v: &chain::Verified<'_>, name: &PackageName<'_>, which: &str,
             release: &Release<'_>, b: &mut Bufs, r: &mut Rows) -> Result<Option<Checked>, Why> {
    let pkg = registry_dir(name, release.version);
    let mut p = pkg;
    p.push("manifest.toml");
    let ml = chain::required(s, base, p.as_str(), &mut b.m, "the manifest")?;
    if godspeed_package::Sha256Digest::of(&b.m[..ml]) != release.manifest {
        return Err(Why::Trust { what: "the manifest", err: godspeed_package::trust::TrustError::DigestMismatch("the manifest") });
    }
    let mut p = pkg;
    p.push("manifest.sig");
    let sl = chain::required(s, base, p.as_str(), &mut b.s, "the manifest's signature")?;
    let sig_text = chain::text(&b.s[..sl], "the manifest's signature")?;
    let signature = match ManifestSignature::parse(sig_text).and_then(|sig| sig.verify(&b.m[..ml]).map(|()| sig)) {
        Ok(sig) => sig,
        Err(e) => {
            r.fail("signature", which, cell(format_args!("{:?} - the manifest is not the publisher's; nothing was kept", e)).as_str());
            return Ok(None);
        }
    };
    v.namespaces.trusts_release(name.publisher(), &signature.key, release.published)
        .map_err(|err| Why::Trust { what: "the publisher's key", err })?;
    let mtext = chain::text(&b.m[..ml], "the manifest")?;
    let mut arts = Artifacts::new();
    let m = match meta::manifest(mtext, &mut arts) {
        Ok(m) => m.manifest,
        Err(e) => { r.fail("manifest", which, cell(format_args!("line {}: {}", e.line, e.why)).as_str()); return Ok(None); }
    };
    if m.name != *name || m.version != release.version {
        r.fail("manifest", which, "the signed manifest names another package or version than the index; nothing was kept");
        return Ok(None);
    }
    let mut p = pkg;
    p.push(godspeed_package::CONTRACT_FILE);
    let cl = chain::required(s, base, p.as_str(), &mut b.c, "the contract")?;
    if godspeed_package::Sha256Digest::of(&b.c[..cl]) != m.contract {
        return Err(Why::Trust { what: "the contract", err: godspeed_package::trust::TrustError::DigestMismatch("the contract") });
    }
    let ctext = chain::text(&b.c[..cl], "the contract")?;
    if !contract_identity(ctext).is_some_and(|(cn, cv)| m.validate_against_contract(cn, cv, |_| {})) {
        r.fail("contract", which, "the contract's name or version is not the package's; nothing was kept");
        return Ok(None);
    }
    Ok(Some(Checked { ml, sl, cl, key: signature.key }))
}

/// The key's standing with the publisher, for a row.
fn key_note(v: &chain::Verified<'_>, publisher: &str, key: &PublicKey) -> Cell {
    match v.namespaces.status(publisher, key) {
        Some(KeyStatus::Retired(at)) => cell(format_args!("a retired key of the publisher's, retired {}, published before then", at)),
        Some(KeyStatus::Revoked(at)) => cell(format_args!("REVOKED {}", at)),
        _ => cell(format_args!("an active key of the publisher's")),
    }
}

/// The architectures a manifest ships, and whether this machine's is one of them.
fn arch_cells(m: &godspeed_package::Manifest<'_>) -> (Cell, Cell) {
    let mut list = Cell::new();
    for (i, a) in m.artifacts.iter().enumerate() {
        let _ = write!(list, "{}{}", if i > 0 { ", " } else { "" }, a.arch.as_str());
    }
    let note = match this_arch() {
        Some(here) if m.artifacts.iter().any(|a| a.arch == here) => cell(format_args!("this machine's, {}, is one", here.as_str())),
        Some(here) => cell(format_args!("NOT this machine's, {}: it cannot be fetched here", here.as_str())),
        None => cell(format_args!("this machine's architecture is not one packages name")),
    };
    (list, note)
}

/// The OS release a package was built against, beside this machine's.
fn os_cells(m: &godspeed_package::Manifest<'_>) -> (Cell, Cell) {
    let built = cell(format_args!("{}", m.os_release));
    let note = if built.as_str() == OS_RELEASE {
        cell(format_args!("the release this machine runs"))
    } else {
        cell(format_args!("this machine runs {}: there is no stable interface yet, so another release may not run it", OS_RELEASE))
    };
    (built, note)
}

/// `godspeed info <publisher/name>[@version]`: a CARD about one release - what it is, its licence, the release
/// it was built for, what it ships, what its contract requests, who signed it, and every version published -
/// each checked as `fetch` checks it. Columns `key value note`; the shell shows it as a card.
#[inline(never)]
pub fn info(ctx: &ServiceContext, r: &mut Rows, ready: &Ready<'_>, roots: &Roots, which: &str) {
    let (name, wanted) = match spec(which) {
        Ok(x) => x,
        Err(why) => return r.fail("usage", which, why),
    };
    let mut docs = Docs::new();
    let base = ready.url.base;
    on_registry(ctx, r, ready, roots, |s, r| {
        let mut iso = [0u8; 20];
        let v = chain::verify(ctx, s, base, &ready.reg, chain::iso(ready.now, &mut iso), &mut docs)?;
        let mut ibuf = [0u8; INDEX_MAX];
        let Some(text) = index_text(ctx, s, base, &v, &name, &mut ibuf)? else {
            r.fail("package", which, "never published on this registry");
            chain_note(r, &v, "");
            return Ok(());
        };
        let mut rel = Releases::new();
        let idx = match meta::index(text, &mut rel) {
            Ok(i) => i,
            Err(b) => { r.fail("index", which, cell(format_args!("line {}: {}", b.line, b.why)).as_str()); return Ok(()); }
        };
        let Some(release) = release_of(&idx, wanted, r) else { return Ok(()) };
        let mut b = Bufs::new();
        let Some(c) = documents(s, base, &v, &name, which, &release, &mut b, r)? else { return Ok(()) };

        // Every request is made: now the card.
        let latest = idx.releases.iter().map(|rel| rel.version).max();
        let mut arts = Artifacts::new();
        let Ok(read) = meta::manifest(chain::text(&b.m[..c.ml], "the manifest")?, &mut arts) else { return Ok(()) };
        let m = read.manifest;
        r.columns(&["key", "value", "note"]);
        r.row("title", cell(format_args!("{} {}", name, release.version)).as_str(),
              cell(format_args!("{}published {} by {}", if Some(release.version) == latest { "latest; " } else { "" },
                  &release.published[..release.published.len().min(10)], release.published_by)).as_str());
        r.row("about", m.description.unwrap_or("(no description)"), "");
        r.row("licence", m.license.unwrap_or("-"), "not interpreted");
        let (built, os_note) = os_cells(&m);
        r.row("built for", built.as_str(), os_note.as_str());
        let (archs, arch_note) = arch_cells(&m);
        r.row("ships", archs.as_str(), arch_note.as_str());
        let mut req = Cell::new();
        requests(chain::text(&b.c[..c.cl], "the contract")?, &mut req);
        r.row("requests", req.as_str(), "what its contract asks for; fetching grants nothing");
        r.row("signed", short_key(&c.key).as_str(), key_note(&v, name.publisher(), &c.key).as_str());
        let mut vers = Cell::new();
        for (i, rel) in idx.releases.iter().enumerate() {
            let _ = write!(vers, "{}{}", if i > 0 { " " } else { "" }, rel.version);
        }
        r.row("versions", vers.as_str(), "");
        let _ = c.sl;
        chain_note_through(r, &v, "", " -> index -> manifest -> signature -> contract");
        Ok(())
    });
}

/// The capabilities a contract REQUESTS, by name, for the answer: the `[capabilities]` keys.
fn requests(contract: &str, out: &mut Cell) {
    let mut table = "";
    for (_, l) in lines(contract) {
        match l {
            Ok(Line::Table(t)) => table = t,
            Ok(Line::Pair(k, _)) if table == "capabilities" => {
                if out.n > 0 { let _ = out.write_str(", "); }
                let _ = out.write_str(k);
            }
            _ => {}
        }
    }
    if out.n == 0 { let _ = out.write_str("nothing"); }
}

/// The contract's own `name` and `version`, top level.
fn contract_identity(contract: &str) -> Option<(&str, &str)> {
    let (mut name, mut version) = (None, None);
    for (_, l) in lines(contract) {
        match l {
            Ok(Line::Table(_)) | Ok(Line::ArrayTable(_)) => break,
            Ok(Line::Pair("name", Val::Str(s))) => name = Some(s),
            Ok(Line::Pair("version", Val::Str(s))) => version = Some(s),
            _ => {}
        }
    }
    Some((name?, version?))
}

/// `/system/packages/<publisher>/<name>/<version>`.
fn local_dir(name: &PackageName<'_>, version: &str) -> Path {
    let mut dest = Path::new();
    dest.push(DIR).push(name.publisher()).push("/").push(name.name()).push("/").push(version);
    dest
}

/// `godspeed fetch <publisher/name>[@version]`: verify everything, and keep it.
#[inline(never)]
pub fn fetch(ctx: &ServiceContext, r: &mut Rows, ready: &Ready<'_>, roots: &Roots, which: &str) {
    let (name, wanted) = match spec(which) {
        Ok(x) => x,
        Err(why) => return r.fail("usage", which, why),
    };
    let Some(arch) = this_arch() else {
        return r.fail("arch", env!("GODSPEED_PM_TARGET"), "no package architecture is named for this machine's target");
    };
    let mut docs = Docs::new();
    let base = ready.url.base;
    on_registry(ctx, r, ready, roots, |s, r| {
        let mut iso = [0u8; 20];
        let now = chain::iso(ready.now, &mut iso);
        crate::progress(ctx, format_args!("  checking {}: root - timestamp - snapshot - namespaces", ready.reg.name));
        let v = chain::verify(ctx, s, base, &ready.reg, now, &mut docs)?;
        crate::progress(ctx, format_args!("  checking {}: index - manifest - signature - contract", name));
        let mut ibuf = [0u8; INDEX_MAX];
        let Some(text) = index_text(ctx, s, base, &v, &name, &mut ibuf)? else {
            r.fail("package", which, "never published on this registry");
            return Ok(());
        };
        let mut rels = Releases::new();
        let idx = match meta::index(text, &mut rels) {
            Ok(i) => i,
            Err(b) => { r.fail("index", which, cell(format_args!("line {}: {}", b.line, b.why)).as_str()); return Ok(()); }
        };
        let Some(release) = release_of(&idx, wanted, r) else { return Ok(()) };
        let mut b = Bufs::new();
        let Some(c) = documents(s, base, &v, &name, which, &release, &mut b, r)? else { return Ok(()) };
        let mut arts = Artifacts::new();
        let Ok(read) = meta::manifest(chain::text(&b.m[..c.ml], "the manifest")?, &mut arts) else { return Ok(()) };
        let m = read.manifest;
        let Some(artifact) = m.artifacts.iter().find(|a| a.arch == arch).copied() else {
            r.fail("arch", arch.as_str(), "the package has no binary for this machine");
            return Ok(());
        };
        if artifact.size > ready.max_bytes {
            r.fail("size", cell(format_args!("{} bytes", artifact.size)).as_str(), "larger than max-package-mib allows");
            return Ok(());
        }
        let vtext = cell(format_args!("{}", release.version));
        let dest = local_dir(&name, vtext.as_str());
        let mut fs = gs::fs::Fs::new(ctx);
        let key = cell(format_args!("{}", c.key));
        let key_status = key_note(&v, name.publisher(), &c.key);
        let mut req = Cell::new();
        requests(chain::text(&b.c[..c.cl], "the contract")?, &mut req);
        let already = fs.exists(dest.as_str()).unwrap_or(false);
        if !already {
            let mut staging = dest;
            staging.push(".partial");
            let _ = fs.delete_all(staging.as_str());
            fs.create_dir_all(staging.as_str()).map_err(Why::Store)?;

            // The binary, streamed to disk and checked as it arrives: never held, never kept unverified.
            let mut bin_rel = registry_dir(&name, release.version);
            bin_rel.push(arch.artifact_file());
            let mut bin = staging;
            bin.push("/").push(arch.artifact_file());
            fs.create_sized(bin.as_str(), artifact.size).map_err(Why::Store)?;
            let mut verifier = ArtifactVerifier::new(artifact);
            let mut chunk = [0u8; gs::fs::IO_CHUNK];
            let (mut held, mut written) = (0usize, 0u64);
            let mut store_err: Option<gs::Error> = None;
            let mut bad_bytes = false;
            let mut path = Path::new();
            path.push(base).push(bin_rel.as_str());
            let mut got = 0u64;
            let mut shown = u64::MAX;
            let file = arch.artifact_file();
            let total = artifact.size;
            let head = s.get(path.as_str(), &mut |bytes: &[u8]| {
                if verifier.update(bytes).is_err() { bad_bytes = true; return false; }
                got += bytes.len() as u64;
                // The bar, redrawn when it moves by a twentieth (and at the end), never per record.
                let step = got * 28 / total.max(1);
                if step != shown || got == total {
                    shown = step;
                    crate::progress(ctx, format_args!("  downloading {}  [{}]  {} / {}   [q] quit", file, bar(step as usize, 28).as_str(),
                        size_text(got).as_str(), size_text(total).as_str()));
                }
                let mut rest = bytes;
                while !rest.is_empty() {
                    let take = rest.len().min(chunk.len() - held);
                    chunk[held..held + take].copy_from_slice(&rest[..take]);
                    held += take;
                    rest = &rest[take..];
                    if held == chunk.len() {
                        if let Err(e) = fs.write_at(bin.as_str(), written, &chunk) { store_err = Some(e); return false; }
                        written += held as u64;
                        held = 0;
                    }
                }
                true
            });
            if bad_bytes {
                let _ = fs.delete_all(staging.as_str());
                r.fail("binary", arch.artifact_file(), "its bytes are not the ones the manifest names; nothing was kept");
                return Ok(());
            }
            if let Some(e) = store_err {
                let _ = fs.delete_all(staging.as_str());
                return Err(Why::Store(e));
            }
            let head = head.map_err(|e| { let _ = fs.delete_all(staging.as_str()); Why::Net(e) })?;
            if head.status != 200 {
                let _ = fs.delete_all(staging.as_str());
                return Err(Why::Missing { path: bin_rel, status: head.status });
            }
            if held > 0 {
                fs.write_at(bin.as_str(), written, &chunk[..held]).map_err(Why::Store)?;
            }
            if verifier.finish().is_err() {
                let _ = fs.delete_all(staging.as_str());
                r.fail("binary", arch.artifact_file(), "it is not the size or digest the manifest names; nothing was kept");
                return Ok(());
            }

            // The documents that prove it, beside it, and where it came from.
            for (file, bytes) in [("manifest.toml", &b.m[..c.ml]), ("manifest.sig", &b.s[..c.sl]), (godspeed_package::CONTRACT_FILE, &b.c[..c.cl])] {
                let mut p = staging;
                p.push("/").push(file);
                fs.write(p.as_str(), bytes).map_err(Why::Store)?;
            }
            let mut origin = Path::new();
            origin.push("registry ").push(ready.reg.name).push(" ").push(ready.reg.url).push("\nfetched ").push(now)
                  .push("\narch ").push(arch.as_str()).push("\nsigned-by ").push(key.as_str())
                  .push("\npublished ").push(release.published).push("\n");
            let mut op = staging;
            op.push("/origin");
            fs.write(op.as_str(), &origin.b[..origin.n]).map_err(Why::Store)?;
            fs.rename(staging.as_str(), vtext.as_str()).map_err(Why::Store)?;
        }
        r.columns(&["key", "value", "note"]);
        r.row("title", cell(format_args!("{} {}", name, release.version)).as_str(),
              cell(format_args!("{} from {}", if already { "already fetched" } else { "fetched" }, ready.reg.name)).as_str());
        r.row("about", dest.as_str(), "verified: kept here, not installed");
        r.row("binary", cell(format_args!("{}, {}", arch.as_str(), size_text(artifact.size).as_str())).as_str(), "its size and digest the manifest's");
        let (built, os_note) = os_cells(&m);
        r.row("built for", built.as_str(), os_note.as_str());
        r.row("requests", req.as_str(), "what its contract asks for; fetching grants nothing");
        r.row("signed", short_key(&c.key).as_str(), key_status.as_str());
        r.row("installed", "no", if m.kind == godspeed_package::Kind::Utility {
            "a utility: `godspeed install` makes it a command, run when typed"
        } else { "`godspeed install` starts it, and at every boot" });
        let _ = key;
        chain_note_through(r, &v, "", " -> index -> manifest -> signature -> contract -> binary");
        Ok(())
    });
}

// ---- what is fetched here --------------------------------------------------------------------------------

/// One fetched version, read from its folder.
#[derive(Clone, Copy)]
pub struct Fetched {
    publisher: [u8; 64], pl: usize,
    name: [u8; 64], nl: usize,
    version: [u8; 32], vl: usize,
    registry: [u8; 32], rl: usize,
    pub bytes: u64,
}

impl Fetched {
    /// An empty slot, for a caller's array.
    pub const EMPTY_PUB: Fetched = Fetched::EMPTY;
    const EMPTY: Fetched = Fetched { publisher: [0; 64], pl: 0, name: [0; 64], nl: 0, version: [0; 32], vl: 0, registry: [0; 32], rl: 0, bytes: 0 };
    fn s(b: &[u8]) -> &str { core::str::from_utf8(b).unwrap_or("?") }
    pub fn publisher(&self) -> &str { Self::s(&self.publisher[..self.pl]) }
    pub fn name(&self) -> &str { Self::s(&self.name[..self.nl]) }
    pub fn version(&self) -> &str { Self::s(&self.version[..self.vl]) }
    pub fn registry(&self) -> &str { Self::s(&self.registry[..self.rl]) }
    fn dir(&self) -> Path {
        let mut p = Path::new();
        p.push(DIR).push(self.publisher()).push("/").push(self.name()).push("/").push(self.version());
        p
    }
}

fn copy_into(dst: &mut [u8], src: &[u8]) -> usize {
    let n = src.len().min(dst.len());
    dst[..n].copy_from_slice(&src[..n]);
    n
}

/// The versions fetched here, in folder order: each with the registry it came from and its size on disk.
/// `Ok(n)` with the first `n` of `out` filled; `more` is set when there were more than `out` holds.
pub fn fetched(ctx: &ServiceContext, out: &mut [Fetched; MAX_FETCHED], more: &mut bool) -> Result<usize, gs::Error> {
    let mut fs = gs::fs::Fs::new(ctx);
    let mut pubs = [[0u8; 64]; 16];
    let mut pl = [0usize; 16];
    let mut npubs = 0usize;
    match fs.list_dir(DIR, |e| { if e.is_dir && npubs < 16 { pl[npubs] = copy_into(&mut pubs[npubs], e.name); npubs += 1; } true }) {
        Ok(_) => {}
        Err(gs::Error::NotFound) => return Ok(0),
        Err(e) => return Err(e),
    }
    let mut n = 0usize;
    for i in 0..npubs {
        let publisher = Fetched::s(&pubs[i][..pl[i]]);
        let mut dir = Path::new();
        dir.push(DIR).push(publisher);
        let mut names = [[0u8; 64]; 32];
        let mut nlen = [0usize; 32];
        let mut nn = 0usize;
        let _ = fs.list_dir(dir.as_str(), |e| { if e.is_dir && nn < 32 { nlen[nn] = copy_into(&mut names[nn], e.name); nn += 1; } true });
        for j in 0..nn {
            let name = Fetched::s(&names[j][..nlen[j]]);
            let mut d2 = dir;
            d2.push("/").push(name);
            let mut vers = [[0u8; 32]; 16];
            let mut vlen = [0usize; 16];
            let mut nv = 0usize;
            let _ = fs.list_dir(d2.as_str(), |e| { if e.is_dir && nv < 16 { vlen[nv] = copy_into(&mut vers[nv], e.name); nv += 1; } true });
            for k in 0..nv {
                let version = Fetched::s(&vers[k][..vlen[k]]);
                if version.ends_with(".partial") { continue; }
                if n == MAX_FETCHED { *more = true; return Ok(n); }
                let mut f = Fetched::EMPTY;
                f.pl = copy_into(&mut f.publisher, publisher.as_bytes());
                f.nl = copy_into(&mut f.name, name.as_bytes());
                f.vl = copy_into(&mut f.version, version.as_bytes());
                let mut vdir = d2;
                vdir.push("/").push(version);
                let mut bytes = 0u64;
                let _ = fs.list_dir(vdir.as_str(), |e| { bytes += e.size; true });
                f.bytes = bytes;
                let mut op = vdir;
                op.push("/origin");
                let mut ob = [0u8; ORIGIN_MAX];
                let got = fs.read_into(op.as_str(), &mut ob).unwrap_or(0);
                let text = core::str::from_utf8(&ob[..got]).unwrap_or("");
                let reg = text.lines().find_map(|l| l.strip_prefix("registry ")).and_then(|l| l.split_whitespace().next()).unwrap_or("?");
                f.rl = copy_into(&mut f.registry, reg.as_bytes());
                out[n] = f;
                n += 1;
            }
        }
    }
    Ok(n)
}

fn size_text(bytes: u64) -> Cell {
    if bytes < 1024 { cell(format_args!("{} B", bytes)) }
    else if bytes < 1024 * 1024 { cell(format_args!("{}.{} KiB", bytes / 1024, bytes % 1024 * 10 / 1024)) }
    else { cell(format_args!("{}.{} MiB", bytes >> 20, (bytes & 0xfffff) * 10 >> 20)) }
}

/// `godspeed list fetched`: what has been fetched here. Columns `package version from size state`, `size` in
/// bytes so a pipe can compare it; the count and total are the status note. None is no rows.
pub fn list_fetched(ctx: &ServiceContext, r: &mut Rows) {
    let mut all = [Fetched::EMPTY; MAX_FETCHED];
    let mut more = false;
    let n = match fetched(ctx, &mut all, &mut more) {
        Ok(n) => n,
        Err(e) => return r.fail("fetched", DIR, cell(format_args!("could not be listed: {:?}", e)).as_str()),
    };
    r.columns(&["package", "version", "from", "size", "state"]);
    let mut total = 0u64;
    for f in all.iter().take(n) {
        let pkg = cell(format_args!("{}/{}", f.publisher(), f.name()));
        let mut iv = [0u8; 24];
        let installed = crate::install::installed_version(ctx, pkg.as_str(), &mut iv).is_some_and(|n| &iv[..n] == f.version().as_bytes());
        r.entry(&[C::S(pkg.as_str()), C::S(f.version()), C::S(f.registry()), C::N(f.bytes),
                  C::S(if installed { "installed" } else { "fetched, not installed" })]);
        total += f.bytes;
    }
    if n == 0 {
        r.note(format_args!("`godspeed fetch <publisher/name>` fetches and verifies one"));
    } else {
        r.note(format_args!("{} package{}, {}{}", n, if n == 1 { "" } else { "s" }, size_text(total).as_str(),
            if more { "; more are fetched than one listing shows" } else { "" }));
    }
}

/// A bar of `width` cells, `filled` of them `#`.
fn bar(filled: usize, width: usize) -> Cell {
    let mut c = Cell::new();
    for i in 0..width { let _ = c.write_str(if i < filled { "#" } else { "-" }); }
    c
}


/// The fetched versions `which` names: every one, one package's, or one version.
fn select(all: &[Fetched], which: Option<&str>) -> Result<[bool; MAX_FETCHED], &'static str> {
    let mut pick = [false; MAX_FETCHED];
    let (name, version) = match which {
        None => (None, None),
        Some(w) => { let (n, v) = spec(w)?; (Some(n), v) }
    };
    for (i, f) in all.iter().enumerate() {
        let name_ok = name.is_none_or(|n| n.publisher() == f.publisher() && n.name() == f.name());
        let ver_ok = version.is_none_or(|v| cell(format_args!("{}", v)).as_str() == f.version());
        pick[i] = name_ok && ver_ok;
    }
    Ok(pick)
}

/// `godspeed verify [<publisher/name>[@version]]`: check fetched copies again from what is on disk, with no
/// network - the signature over the manifest, the manifest against its folder, the contract and the binary
/// against the manifest's digests and sizes. Whether the registry still vouches for the key needs the network
/// (`godspeed list updates` asks it); this is about the bytes on this disk.
#[inline(never)]
pub fn verify(ctx: &ServiceContext, r: &mut Rows, which: Option<&str>) {
    let mut all = [Fetched::EMPTY; MAX_FETCHED];
    let mut more = false;
    let n = match fetched(ctx, &mut all, &mut more) {
        Ok(n) => n,
        Err(e) => return r.fail("fetched", DIR, cell(format_args!("could not be listed: {:?}", e)).as_str()),
    };
    let pick = match select(&all[..n], which) {
        Ok(p) => p,
        Err(why) => return r.fail("usage", which.unwrap_or(""), why),
    };
    let mut checked = 0u32;
    let mut damaged = 0u32;
    r.columns(&["package", "version", "status", "detail"]);
    for (i, f) in all.iter().take(n).enumerate() {
        if !pick[i] { continue; }
        checked += 1;
        let pkg = cell(format_args!("{}/{}", f.publisher(), f.name()));
        match verify_one(ctx, f) {
            Ok(arch) => r.entry(&[C::S(pkg.as_str()), C::S(f.version()), C::S("intact"), C::S(cell(format_args!(
                "the signature, the contract and the {} binary match the manifest", arch)).as_str())]),
            Err(why) => { damaged += 1; r.entry(&[C::S(pkg.as_str()), C::S(f.version()), C::S("DAMAGED"), C::S(why.as_str())]); }
        }
    }
    // A damaged copy fails the command, in the list's own shape: the rows say which.
    if damaged > 0 { r.failed_in_place(); }
    if checked == 0 {
        if which.is_some() {
            return r.fail("fetched", which.unwrap_or(""), "not fetched here: `godspeed list fetched` says what is");
        }
        r.note(format_args!("nothing is fetched here"));
        return;
    }
    r.note(format_args!("{} checked, {} damaged; from the disk alone - `godspeed fetch` again replaces a damaged copy once it is removed",
        checked, damaged));
}

/// One fetched version, checked from disk. The architecture of its binary, or what is wrong.
fn verify_one(ctx: &ServiceContext, f: &Fetched) -> Result<&'static str, Cell> {
    crate::check::package(ctx, f.publisher(), f.name(), f.version(), None, &mut |_, _| true)
        .map(|c| c.arch)
        .map_err(|why| cell(format_args!("{}", why.as_str())))
}

/// `godspeed list updates`, for the fetched packages from one registry: the newest version each has there,
/// against the newest fetched here. Rows only for packages from `ready`'s registry; the caller runs this
/// once per registry the fetched packages came from.
#[inline(never)]
pub fn updates(ctx: &ServiceContext, r: &mut Rows, ready: &Ready<'_>, roots: &Roots, all: &[Fetched]) {
    let mut docs = Docs::new();
    let base = ready.url.base;
    on_registry(ctx, r, ready, roots, |s, r| {
        let mut iso = [0u8; 20];
        let v = chain::verify(ctx, s, base, &ready.reg, chain::iso(ready.now, &mut iso), &mut docs)?;
        // One answer per package: the newest version fetched here.
        let mut seen = [false; MAX_FETCHED];
        let mut answers: [(usize, Option<Version>, u8); MAX_FETCHED] = [(0, None, 0); MAX_FETCHED];
        let mut na = 0usize;
        for i in 0..all.len() {
            if seen[i] || all[i].registry() != ready.reg.name { continue; }
            let mut best = i;
            for j in i + 1..all.len() {
                if all[j].publisher() == all[i].publisher() && all[j].name() == all[i].name() {
                    seen[j] = true;
                    let (a, b) = (all[best].version().parse::<Version>().ok(), all[j].version().parse::<Version>().ok());
                    if b > a { best = j; }
                }
            }
            let full = cell(format_args!("{}/{}", all[i].publisher(), all[i].name()));
            let Ok(name) = PackageName::parse(full.as_str()) else { continue };
            let mut ibuf = [0u8; INDEX_MAX];
            let (latest, state) = match index_text(ctx, s, base, &v, &name, &mut ibuf)? {
                None => (None, 1u8),
                Some(text) => {
                    let mut rels = Releases::new();
                    match meta::index(text, &mut rels) {
                        Ok(idx) => (idx.latest().map(|l| l.version), 0u8),
                        Err(_) => (None, 2u8),
                    }
                }
            };
            answers[na] = (best, latest, state);
            na += 1;
        }
        r.columns(&["package", "have", "latest", "status"]);
        for &(i, latest, state) in answers.iter().take(na) {
            let f = &all[i];
            let label = cell(format_args!("{}/{}", f.publisher(), f.name()));
            match (state, latest) {
                (1, _) => r.entry(&[C::S(label.as_str()), C::S(f.version()), C::S("-"),
                    C::S(cell(format_args!("{} no longer lists it", ready.reg.name)).as_str())]),
                (2, _) | (_, None) => r.entry(&[C::S(label.as_str()), C::S(f.version()), C::S("-"), C::S("its index on the registry does not read")]),
                (_, Some(l)) => {
                    let lt = cell(format_args!("{}", l));
                    let newer = f.version().parse::<Version>().map(|h| l > h).unwrap_or(false);
                    let st = if newer {
                        cell(format_args!("newer on {}: `godspeed fetch {}`", ready.reg.name, label.as_str()))
                    } else {
                        cell(format_args!("up to date on {}", ready.reg.name))
                    };
                    r.entry(&[C::S(label.as_str()), C::S(f.version()), C::S(lt.as_str()), C::S(st.as_str())]);
                }
            }
        }
        chain_note(r, &v, "");
        Ok(())
    });
}

/// `godspeed remove <publisher/name>[@version]`: delete a fetched copy, one version or every one. Nothing of
/// the package's own data is touched: a fetched package has run nowhere, so it has none.
pub fn remove(ctx: &ServiceContext, r: &mut Rows, which: &str) {
    let (name, version) = match spec(which) {
        Ok(x) => x,
        Err(why) => return r.fail("usage", which, why),
    };
    let mut fs = gs::fs::Fs::new(ctx);
    let mut dir = Path::new();
    dir.push(DIR).push(name.publisher()).push("/").push(name.name());
    let mut target = dir;
    if let Some(v) = version {
        let vt = cell(format_args!("{}", v));
        target.push("/").push(vt.as_str());
    }
    // An installed copy is the one the supervisor starts at boot: uninstall it first.
    let mut iv = [0u8; 24];
    let pkg = cell(format_args!("{}", name));
    if let Some(n) = crate::install::installed_version(ctx, pkg.as_str(), &mut iv) {
        let vt = version.map(|v| cell(format_args!("{}", v)));
        if vt.as_ref().is_none_or(|v| v.as_str().as_bytes() == &iv[..n]) {
            return r.fail("installed", which, "it is installed: `godspeed uninstall` it first");
        }
    }
    match fs.exists(target.as_str()) {
        Ok(true) => {}
        Ok(false) => return r.fail("fetched", which, "not fetched here: `godspeed list fetched` says what is"),
        Err(e) => return r.fail("fetched", which, cell(format_args!("could not be checked: {:?}", e)).as_str()),
    }
    if let Err(e) = fs.delete_all(target.as_str()) {
        return r.fail("removed", target.as_str(), cell(format_args!("{:?} - {}", e, fs.reason())).as_str());
    }
    // Empty folders above it go too, so a listing shows only what is there.
    let empty = |p: &str, fs: &mut gs::fs::Fs<'_>| {
        let mut any = false;
        let _ = fs.list_dir(p, |_| { any = true; false });
        if !any { let _ = fs.delete(p); }
    };
    if version.is_some() { empty(dir.as_str(), &mut fs); }
    let mut pubdir = Path::new();
    pubdir.push(DIR).push(name.publisher());
    empty(pubdir.as_str(), &mut fs);
    r.row("removed", target.as_str(), "the fetched copy, and nothing else: a package's own data is not kept here");
}
