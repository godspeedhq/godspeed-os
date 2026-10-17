// SPDX-License-Identifier: GPL-2.0-only
//! Packages: search, inspect, fetch-and-keep, list and remove (`docs/developer-services-design.md` 10.1).
//!
//! **Kept is not installed.** A fetched package is verified and kept under `/system/packages/` (10.5): its
//! signed manifest, its signature, its service contract and this machine's binary, each checked against what
//! the manifest names. This machine cannot start a program from disk yet (the supervisor runs only the images
//! built into it; 9.2), so nothing here runs one, and every answer says so rather than calling it installed.
//!
//! **Every answer is verified first.** Search reads the verified snapshot; info reads an index the snapshot
//! names by digest; fetch reads a manifest the index names by digest, signed by a key the signed namespaces
//! vouch for at the release's publication time, and files the manifest names by digest and size
//! (`chain.rs`, `godspeed_package`). A file that does not match is never kept.
//!
//! **A command's rows are written after its last request**, so when the server closes a kept-alive
//! connection part way (`Fail::Closed`) the command runs again on a fresh one without saying anything twice.

use core::fmt::Write as _;

use godspeed::{self as gs, ServiceContext};
use godspeed_package::{trust::KeyStatus, Arch, ArtifactVerifier, ManifestSignature, PackageName, Version};

use crate::chain::{self, Docs, Path, Why};
use crate::config;
use crate::https::{self, Fail, Session};
use crate::meta::{self, Artifacts, Releases};
use crate::toml_lines::{lines, Line, Val};
use crate::{cell, Cell, Roots, Rows};

/// Where kept packages live: one folder per publisher, package and version (10.5).
pub const DIR: &str = "/system/packages/";
// Sized to what each holds, as `chain.rs`'s are: `godspeed/hello`'s manifest is 821 bytes and its contract
// 439, and an index grows by about 200 bytes a release.
const INDEX_MAX: usize = 8 * 1024;
const MANIFEST_MAX: usize = 4 * 1024;
const MSIG_MAX: usize = 1024;
const CONTRACT_MAX: usize = 4 * 1024;
/// How many times a command is tried on a fresh connection when the server closed the last one part way.
const ATTEMPTS: u32 = 3;

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

/// Run `f` on a session to the registry in use, again on a fresh one when the server closed the last part
/// way, and turn what went wrong into a row.
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
        Why::Net(f) => {
            let (step, c) = crate::failure(f, config::DEFAULT_ROOTS, "the clock");
            (step, c)
        }
        Why::Missing { path, status } => ("registry", cell(format_args!("{} answered HTTP {}", path.as_str(), status))),
        Why::TooLarge(what) => ("registry", cell(format_args!("{} is larger than this machine reads", what))),
        Why::NotText(what) => ("registry", cell(format_args!("{} is not text", what))),
        Why::Trust { what, err } => ("trust", cell(format_args!("{}: {} - nothing was kept", what, err))),
        Why::PinMismatch => ("trust", cell(format_args!(
            "the registry's root 1 is not the root {} pins ({}) - nothing from it is trusted", ready.reg.name, ready.reg.pin))),
        Why::PinChanged => ("trust", cell(format_args!(
            "the pin for {} changed since this machine first reached it; remove {}{}/ to trust the new one",
            ready.reg.name, "/system/services/godspeed/trust/", ready.reg.name))),
        Why::Store(e) => ("trust", cell(format_args!("this machine's record of the registry: {:?}", e))),
    }
}

/// The chain's versions, for a row.
fn chain_row(r: &mut Rows, v: &chain::Verified<'_>) {
    r.row("verified", cell(format_args!("root {}, timestamp {}, snapshot {}, namespaces {}",
        v.root.header.version, v.timestamp.header.version, v.snapshot.header.version, v.namespaces.header.version)).as_str(),
        if v.first_contact { "first contact: root 1 matched the pin" } else { "against the root this machine already trusts" });
}

/// `godspeed search [<word>]`: every package the verified snapshot lists, or those whose name holds `word`.
#[inline(never)]
pub fn search(ctx: &ServiceContext, r: &mut Rows, ready: &Ready<'_>, roots: &Roots, word: Option<&str>) {
    let mut docs = Docs::new();
    let base = ready.url.base;
    on_registry(ctx, r, ready, roots, |s, r| {
        let mut iso = [0u8; 20];
        let v = chain::verify(ctx, s, base, &ready.reg, chain::iso(ready.now, &mut iso), &mut docs)?;
        chain_row(r, &v);
        let mut shown = 0u32;
        for name in chain::packages(&v) {
            if word.is_some_and(|w| !name.contains(w)) { continue; }
            r.row("package", name, "`godspeed info <name>` lists its versions");
            shown += 1;
        }
        if shown == 0 {
            r.row("package", "none", if word.is_some() { "no package's name holds that word" } else { "the registry lists none" });
        }
        Ok(())
    });
}

/// `godspeed info <publisher/name>`: every release the verified index lists.
#[inline(never)]
pub fn info(ctx: &ServiceContext, r: &mut Rows, ready: &Ready<'_>, roots: &Roots, which: &str) {
    let (name, _) = match spec(which) {
        Ok(x) => x,
        Err(why) => return r.fail("usage", which, why),
    };
    let mut docs = Docs::new();
    let base = ready.url.base;
    on_registry(ctx, r, ready, roots, |s, r| {
        let mut iso = [0u8; 20];
        let v = chain::verify(ctx, s, base, &ready.reg, chain::iso(ready.now, &mut iso), &mut docs)?;
        let mut buf = [0u8; INDEX_MAX];
        let Some(text) = index_text(s, base, &v, &name, &mut buf)? else {
            chain_row(r, &v);
            r.fail("package", which, "never published on this registry");
            return Ok(());
        };
        let mut rel = Releases::new();
        let idx = match meta::index(text, &mut rel) {
            Ok(i) => i,
            Err(b) => {
                r.fail("index", which, cell(format_args!("line {}: {}", b.line, b.why)).as_str());
                return Ok(());
            }
        };
        chain_row(r, &v);
        let latest = idx.latest().map(|l| l.version);
        for rel in idx.releases {
            let c = cell(format_args!("{}", rel.version));
            let note = cell(format_args!("published {} by {}{}", rel.published, rel.published_by,
                if Some(rel.version) == latest { "; the latest" } else { "" }));
            r.row("version", c.as_str(), note.as_str());
        }
        Ok(())
    });
}

/// The package's index, verified against the snapshot, or `None` if the registry has never published it.
fn index_text<'b>(s: &mut Session<'_, '_, '_, '_>, base: &str, v: &chain::Verified<'_>, name: &PackageName<'_>,
                  buf: &'b mut [u8; INDEX_MAX]) -> Result<Option<&'b str>, Why> {
    let mut rel = Path::new();
    rel.push("index/").push(name.publisher()).push("/").push(name.name()).push(".toml");
    let full = cell(format_args!("{}", name));
    if v.snapshot.index(full.as_str()).is_none() {
        return Ok(None);
    }
    let len = chain::required(s, base, rel.as_str(), buf, "the package index")?;
    let text = chain::text(&buf[..len], "the package index")?;
    godspeed_package::trust::verify_index(&v.snapshot, full.as_str(), text)
        .map_err(|err| Why::Trust { what: "the package index", err })?;
    Ok(Some(text))
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

/// What one fetch decided, gathered before any row is written.
struct Fetched {
    dest: Path,
    already: bool,
    key: Cell,
    retired: Option<Cell>,
    requests: Cell,
    arch: &'static str,
    size: u64,
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
        let v = chain::verify(ctx, s, base, &ready.reg, now, &mut docs)?;

        // The release, from the verified index.
        let mut ibuf = [0u8; INDEX_MAX];
        let Some(text) = index_text(s, base, &v, &name, &mut ibuf)? else {
            r.fail("package", which, "never published on this registry");
            return Ok(());
        };
        let mut rels = Releases::new();
        let idx = match meta::index(text, &mut rels) {
            Ok(i) => i,
            Err(b) => { r.fail("index", which, cell(format_args!("line {}: {}", b.line, b.why)).as_str()); return Ok(()); }
        };
        let release = match wanted {
            Some(want) => match idx.release(want) {
                Some(rel) => *rel,
                None => {
                    let mut all = Cell::new();
                    for (i, rel) in idx.releases.iter().enumerate() {
                        let _ = write!(all, "{}{}", if i > 0 { ", " } else { "" }, rel.version);
                    }
                    r.fail("version", cell(format_args!("{}", want)).as_str(), cell(format_args!("not published; these are: {}", all.as_str())).as_str());
                    return Ok(());
                }
            },
            None => match idx.latest() { Some(rel) => *rel, None => return Ok(()) },
        };

        // The manifest, by the digest the index names; its signature, by a key the namespaces vouch for.
        let mut pkg = Path::new();
        pkg.push("packages/").push(name.publisher()).push("/").push(name.name()).push("/");
        let vtext = cell(format_args!("{}", release.version));
        pkg.push(vtext.as_str()).push("/");
        let mut mbuf = [0u8; MANIFEST_MAX];
        let mut rel_path = pkg;
        rel_path.push("manifest.toml");
        let mlen = chain::required(s, base, rel_path.as_str(), &mut mbuf, "the manifest")?;
        if godspeed_package::Sha256Digest::of(&mbuf[..mlen]) != release.manifest {
            return Err(Why::Trust { what: "the manifest", err: godspeed_package::trust::TrustError::DigestMismatch("the manifest") });
        }
        let mut sbuf = [0u8; MSIG_MAX];
        let mut sig_path = pkg;
        sig_path.push("manifest.sig");
        let slen = chain::required(s, base, sig_path.as_str(), &mut sbuf, "the manifest's signature")?;
        let sig_text = chain::text(&sbuf[..slen], "the manifest's signature")?;
        let signature = match ManifestSignature::parse(sig_text).and_then(|sig| sig.verify(&mbuf[..mlen]).map(|()| sig)) {
            Ok(sig) => sig,
            Err(e) => { r.fail("signature", which, cell(format_args!("{:?} - the manifest is not the publisher's; nothing was kept", e)).as_str()); return Ok(()); }
        };
        v.namespaces.trusts_release(name.publisher(), &signature.key, release.published)
            .map_err(|err| Why::Trust { what: "the publisher's key", err })?;
        let mtext = chain::text(&mbuf[..mlen], "the manifest")?;
        let mut arts = Artifacts::new();
        let read = match meta::manifest(mtext, &mut arts) {
            Ok(m) => m,
            Err(b) => { r.fail("manifest", which, cell(format_args!("line {}: {}", b.line, b.why)).as_str()); return Ok(()); }
        };
        let m = read.manifest;
        if m.name != name || m.version != release.version {
            r.fail("manifest", which, "the signed manifest names another package or version than the index; nothing was kept");
            return Ok(());
        }
        let Some(artifact) = m.artifacts.iter().find(|a| a.arch == arch).copied() else {
            r.fail("arch", arch.as_str(), "the package has no binary for this machine");
            return Ok(());
        };
        if artifact.size > ready.max_bytes {
            r.fail("size", cell(format_args!("{} bytes", artifact.size)).as_str(), "larger than max-package-mib allows");
            return Ok(());
        }

        // The contract, by its digest, and against the manifest's name and version.
        let mut cbuf = [0u8; CONTRACT_MAX];
        let mut c_path = pkg;
        c_path.push(godspeed_package::CONTRACT_FILE);
        let clen = chain::required(s, base, c_path.as_str(), &mut cbuf, "the contract")?;
        if godspeed_package::Sha256Digest::of(&cbuf[..clen]) != m.contract {
            return Err(Why::Trust { what: "the contract", err: godspeed_package::trust::TrustError::DigestMismatch("the contract") });
        }
        let ctext = chain::text(&cbuf[..clen], "the contract")?;
        let agrees = contract_identity(ctext).is_some_and(|(cn, cv)| m.validate_against_contract(cn, cv, |_| {}));
        if !agrees {
            r.fail("contract", which, "the contract's name or version is not the package's; nothing was kept");
            return Ok(());
        }

        // Where it goes. A version already kept is left as it is.
        let mut dest = Path::new();
        dest.push(DIR).push(name.publisher()).push("/").push(name.name()).push("/").push(vtext.as_str());
        let mut fs = gs::fs::Fs::new(ctx);
        let key = cell(format_args!("{}", signature.key));
        let retired = match v.namespaces.status(name.publisher(), &signature.key) {
            Some(KeyStatus::Retired(at)) => Some(cell(format_args!("{}", at))),
            _ => None,
        };
        let mut req = Cell::new();
        requests(ctext, &mut req);
        if fs.exists(dest.as_str()).unwrap_or(false) {
            return finish(r, which, &v, Fetched { dest, already: true, key, retired, requests: req, arch: arch.as_str(), size: artifact.size }, ready);
        }
        let mut staging = dest;
        staging.push(".partial");
        let _ = fs.delete_all(staging.as_str());
        fs.create_dir_all(staging.as_str()).map_err(Why::Store)?;

        // The binary, streamed to disk and checked as it arrives: never held, never kept unverified.
        let mut bin_rel = pkg;
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
        let head = s.get(path.as_str(), &mut |b: &[u8]| {
            if verifier.update(b).is_err() { bad_bytes = true; return false; }
            let mut rest = b;
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
        for (file, bytes) in [("manifest.toml", &mbuf[..mlen]), ("manifest.sig", &sbuf[..slen]), (godspeed_package::CONTRACT_FILE, &cbuf[..clen])] {
            let mut p = staging;
            p.push("/").push(file);
            fs.write(p.as_str(), bytes).map_err(Why::Store)?;
        }
        let mut origin = Path::new();
        origin.push("registry ").push(ready.reg.name).push(" ").push(ready.reg.url).push("\nfetched ").push(now)
              .push("\narch ").push(arch.as_str()).push("\nsigned-by ").push(key.as_str()).push("\n");
        let mut op = staging;
        op.push("/origin");
        fs.write(op.as_str(), &origin.b[..origin.n]).map_err(Why::Store)?;
        fs.rename(staging.as_str(), vtext.as_str()).map_err(Why::Store)?;
        finish(r, which, &v, Fetched { dest, already: false, key, retired, requests: req, arch: arch.as_str(), size: artifact.size }, ready)
    });
}

fn finish(r: &mut Rows, which: &str, v: &chain::Verified<'_>, f: Fetched, ready: &Ready<'_>) -> Result<(), Why> {
    chain_row(r, v);
    r.row("package", which, cell(format_args!("from {}", ready.reg.name)).as_str());
    r.row("signed", f.key.as_str(), match &f.retired {
        Some(at) => cell(format_args!("a retired key of the publisher's, retired {}, published before then", at.as_str())),
        None => cell(format_args!("an active key of the publisher's")),
    }.as_str());
    r.row("binary", f.arch, cell(format_args!("{} bytes, its size and digest the manifest's", f.size)).as_str());
    r.row("requests", f.requests.as_str(), "what its contract asks for; nothing is granted by fetching it");
    r.row(if f.already { "already kept" } else { "kept" }, f.dest.as_str(), "verified");
    r.row("installed", "no", "this machine cannot start a program from disk yet (design 9.2): kept, not running");
    Ok(())
}

/// The kept packages under `/system/packages/`: `(publisher, name, version)` for each, in folder order.
fn each_kept(ctx: &ServiceContext, mut f: impl FnMut(&str, &str, &str)) -> Result<(), gs::Error> {
    let mut fs = gs::fs::Fs::new(ctx);
    let mut pubs = [[0u8; 64]; 16];
    let mut npubs = 0usize;
    fs.list_dir(DIR, |e| { if e.is_dir && npubs < 16 { let n = e.name.len().min(64); pubs[npubs][..n].copy_from_slice(&e.name[..n]); npubs += 1; } true })?;
    for p in pubs.iter().take(npubs) {
        let publisher = core::str::from_utf8(p).unwrap_or("").trim_end_matches('\0');
        let mut dir = Path::new();
        dir.push(DIR).push(publisher);
        let mut names = [[0u8; 64]; 32];
        let mut nn = 0usize;
        let _ = fs.list_dir(dir.as_str(), |e| { if e.is_dir && nn < 32 { let n = e.name.len().min(64); names[nn][..n].copy_from_slice(&e.name[..n]); nn += 1; } true });
        for nm in names.iter().take(nn) {
            let name = core::str::from_utf8(nm).unwrap_or("").trim_end_matches('\0');
            let mut d2 = dir;
            d2.push("/").push(name);
            let mut vers = [[0u8; 32]; 16];
            let mut nv = 0usize;
            let _ = fs.list_dir(d2.as_str(), |e| { if e.is_dir && nv < 16 { let n = e.name.len().min(32); vers[nv][..n].copy_from_slice(&e.name[..n]); nv += 1; } true });
            for v in vers.iter().take(nv) {
                let version = core::str::from_utf8(v).unwrap_or("").trim_end_matches('\0');
                if version.ends_with(".partial") { continue; }
                f(publisher, name, version);
            }
        }
    }
    Ok(())
}

/// `godspeed list`: what is kept here, and where it came from.
pub fn list(ctx: &ServiceContext, r: &mut Rows) {
    let mut count = 0u32;
    let mut origins = [[0u8; 256]; 1];
    let walked = each_kept(ctx, |p, n, v| {
        let mut path = Path::new();
        path.push(DIR).push(p).push("/").push(n).push("/").push(v).push("/origin");
        let got = gs::fs::Fs::new(ctx).read_into(path.as_str(), &mut origins[0]).unwrap_or(0);
        let text = core::str::from_utf8(&origins[0][..got]).unwrap_or("");
        let reg = text.lines().find_map(|l| l.strip_prefix("registry ")).and_then(|l| l.split_whitespace().next()).unwrap_or("?");
        r.row(cell(format_args!("{}/{}@{}", p, n, v)).as_str(), cell(format_args!("from {}", reg)).as_str(), "kept, not installed");
        count += 1;
    });
    match walked {
        Ok(()) if count == 0 => r.row("kept", "none", "`godspeed fetch <publisher/name>` keeps a verified copy"),
        Ok(()) => {}
        Err(gs::Error::NotFound) => r.row("kept", "none", "`godspeed fetch <publisher/name>` keeps a verified copy"),
        Err(e) => r.fail("kept", DIR, cell(format_args!("could not be listed: {:?}", e)).as_str()),
    }
}

/// `godspeed uninstall <publisher/name>[@version]`: remove the kept copy, one version or every one. Nothing
/// of the package's own data is touched: a kept package has run nowhere, so it has none.
pub fn uninstall(ctx: &ServiceContext, r: &mut Rows, which: &str) {
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
    match fs.exists(target.as_str()) {
        Ok(true) => {}
        Ok(false) => return r.fail("kept", which, "not kept here: `godspeed list` says what is"),
        Err(e) => return r.fail("kept", which, cell(format_args!("could not be checked: {:?}", e)).as_str()),
    }
    if let Err(e) = fs.delete_all(target.as_str()) {
        return r.fail("removed", target.as_str(), cell(format_args!("{:?} - {}", e, fs.reason())).as_str());
    }
    // Empty folders above it go too, so `list` shows only what is there.
    let empty = |p: &str, fs: &mut gs::fs::Fs<'_>| {
        let mut any = false;
        let _ = fs.list_dir(p, |_| { any = true; false });
        if !any { let _ = fs.delete(p); }
    };
    if version.is_some() { empty(dir.as_str(), &mut fs); }
    let mut pubdir = Path::new();
    pubdir.push(DIR).push(name.publisher());
    empty(pubdir.as_str(), &mut fs);
    r.row("removed", target.as_str(), "the kept copy; it had not run, so it has no data to keep");
}
