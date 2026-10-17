// SPDX-License-Identifier: GPL-2.0-only
//! A fetched package, checked from the disk alone.
//!
//! The signature over its manifest, the signing key's standing in this machine's trust record, the contract
//! and the binary against what the manifest names, and what the contract requests. `godspeed verify` and
//! `godspeed install` run it in `godspeed-pm`; the supervisor runs it AGAIN, by `#[path]`, before it starts the
//! image, and starts the very bytes this read and hashed rather than reading the file a second time. One
//! module, so the two can never disagree about what "intact" means.
//!
//! **What it does not do: reach the registry.** The trust record it reads (`namespaces.toml`, beside the
//! root) was verified against the registry's signed chain when `godspeed` wrote it. Here it is taken as
//! written, so it is exactly as protected as `/system` is - recorded in `docs/developer-services-design.md` 12.

use godspeed as gs;
use godspeed_package::trust::{KeyStatus, Namespaces};
use godspeed_package::{ArtifactVerifier, Kind, ManifestSignature, Sha256Digest};
use gs::ServiceContext;

use crate::meta::{self, Artifacts};
use crate::toml_lines::{lines, strings, Line, Val};

/// Where fetched packages are kept: `<DIR><publisher>/<name>/<version>/`.
pub const DIR: &str = "/system/packages/";
/// Where each registry's trust record is kept: `<TRUST><registry>/`.
pub const TRUST: &str = "/system/services/godspeed/trust/";

pub const MANIFEST_MAX: usize = 4 * 1024;
pub const MSIG_MAX: usize = 1024;
pub const CONTRACT_MAX: usize = 4 * 1024;
pub const ORIGIN_MAX: usize = 512;
pub const NAMESPACES_MAX: usize = 8 * 1024;
/// The most send peers a contract may request and still be installed.
pub const MAX_PEERS: usize = 8;
/// The longest peer name kept.
pub const PEER_MAX: usize = 32;

/// A sentence, bounded, for a row or a log line.
pub struct Text { b: [u8; 160], n: usize }
impl Text {
    pub fn new() -> Text { Text { b: [0; 160], n: 0 } }
    pub fn as_str(&self) -> &str { core::str::from_utf8(&self.b[..self.n]).unwrap_or("") }
}
impl core::fmt::Write for Text {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let take = s.len().min(self.b.len() - self.n);
        self.b[self.n..self.n + take].copy_from_slice(&s.as_bytes()[..take]);
        self.n += take;
        Ok(())
    }
}
fn text(args: core::fmt::Arguments<'_>) -> Text {
    let mut t = Text { b: [0; 160], n: 0 };
    let _ = core::fmt::Write::write_fmt(&mut t, args);
    t
}

/// What a contract requests that installing it needs to know.
pub struct Contract {
    peers: [[u8; PEER_MAX]; MAX_PEERS],
    lens: [u8; MAX_PEERS],
    np: usize,
    /// It has `ipc_receive`: it gets an endpoint of its own.
    pub receives: bool,
    /// `[resources.memory] limit`, in bytes; 0 when the contract names none.
    pub memory_limit: u64,
    /// `hw_pci_class`: the device a driver drives, as the 24-bit PCI class code; `None` for anything else.
    pub pci_class: Option<u32>,
    /// `hw_pci_bar`: which BAR holds its registers - an index, or 7 for "the first memory BAR" (`auto`).
    pub pci_bar: u32,
    /// `hw_pci_irq`: it asks for an interrupt.
    pub pci_irq: bool,
}

impl Contract {
    /// The send peers it requests, in the contract's order.
    pub fn peers(&self) -> impl Iterator<Item = &str> {
        (0..self.np).map(move |i| core::str::from_utf8(&self.peers[i][..self.lens[i] as usize]).unwrap_or(""))
    }
}

/// A fetched package that checked out.
pub struct Checked {
    /// The architecture of the binary that was checked.
    pub arch: &'static str,
    /// Its size in bytes.
    pub size: u64,
    /// What its contract requests.
    pub contract: Contract,
    /// What kind of program it is: a service, kept running, or a utility, run for one command line.
    pub kind: Kind,
}

/// `32MiB`, `512KiB`, `1GiB` or a plain number of bytes.
fn size_of(s: &str) -> Option<u64> {
    let (num, mul) = if let Some(n) = s.strip_suffix("GiB") { (n, 1u64 << 30) }
        else if let Some(n) = s.strip_suffix("MiB") { (n, 1 << 20) }
        else if let Some(n) = s.strip_suffix("KiB") { (n, 1 << 10) }
        else { (s, 1) };
    num.trim().parse::<u64>().ok()?.checked_mul(mul)
}

/// The contract's name, version, send peers, receive endpoint and memory limit.
fn contract(c: &str) -> Result<(&str, &str, Contract), Text> {
    let mut out = Contract { peers: [[0; PEER_MAX]; MAX_PEERS], lens: [0; MAX_PEERS], np: 0, receives: false, memory_limit: 0,
                             pci_class: None, pci_bar: 7, pci_irq: false };
    let (mut name, mut version) = (None, None);
    let mut table = "";
    for (n, l) in lines(c) {
        let l = l.map_err(|why| text(format_args!("contract.toml line {}: {}", n, why)))?;
        match l {
            Line::Table(t) | Line::ArrayTable(t) => table = t,
            Line::Pair("name", Val::Str(s)) if table.is_empty() => name = Some(s),
            Line::Pair("version", Val::Str(s)) if table.is_empty() => version = Some(s),
            Line::Pair("ipc_send", Val::Strings(inner)) if table == "capabilities" => {
                for p in strings(inner) {
                    if out.np == MAX_PEERS { return Err(text(format_args!("its contract requests more than {} send peers", MAX_PEERS))); }
                    if p.is_empty() || p.len() > PEER_MAX { return Err(text(format_args!("its contract names a peer this cannot hold: `{}`", p))); }
                    out.peers[out.np][..p.len()].copy_from_slice(p.as_bytes());
                    out.lens[out.np] = p.len() as u8;
                    out.np += 1;
                }
            }
            Line::Pair("ipc_receive", Val::Strings(inner)) if table == "capabilities" => out.receives = strings(inner).next().is_some(),
            Line::Pair("hw_pci_class", Val::Str(s)) if table == "capabilities" => {
                if s.len() != 6 { return Err(text(format_args!("contract.toml line {}: hw_pci_class is six hex digits", n))); }
                out.pci_class = Some(u32::from_str_radix(s, 16)
                    .map_err(|_| text(format_args!("contract.toml line {}: hw_pci_class is six hex digits", n)))?);
            }
            Line::Pair("hw_pci_bar", Val::Int(b)) if table == "capabilities" && b <= 5 => out.pci_bar = b as u32,
            Line::Pair("hw_pci_bar", Val::Str("auto")) if table == "capabilities" => out.pci_bar = 7,
            Line::Pair("hw_pci_irq", Val::Bool(b)) if table == "capabilities" => out.pci_irq = b,
            Line::Pair("limit", Val::Str(s)) if table == "resources.memory" => {
                out.memory_limit = size_of(s).ok_or_else(|| text(format_args!("contract.toml line {}: `{}` is not a size", n, s)))?;
            }
            _ => {}
        }
    }
    match (name, version) {
        (Some(n), Some(v)) => Ok((n, v, out)),
        _ => Err(text(format_args!("contract.toml has no top-level name and version"))),
    }
}

/// Check `<DIR><publisher>/<name>/<version>` from the disk.
///
/// `arch`, when given, is the only architecture accepted - the supervisor passes this machine's, since an image
/// for another cannot run here; `godspeed verify` passes `None` and checks the one recorded at fetch. Every
/// chunk of the binary is handed to `image` as it is read and hashed, at its offset; `image` answers false when
/// it cannot hold it, which fails the check. A binary is accepted only when every byte the manifest names
/// arrived, so what `image` was given is exactly what was checked.
#[inline(never)]
pub fn package(ctx: &ServiceContext, publisher: &str, name: &str, version: &str, arch: Option<&str>,
               image: &mut dyn FnMut(u64, &[u8]) -> bool) -> Result<Checked, Text> {
    let mut fs = gs::fs::Fs::new(ctx);
    let mut dir = [0u8; 200];
    let dl = {
        let mut w = Text { b: [0; 160], n: 0 };
        let _ = core::fmt::Write::write_fmt(&mut w, format_args!("{}{}/{}/{}/", DIR, publisher, name, version));
        if w.n >= w.b.len() { return Err(text(format_args!("the package's folder name is too long"))); }
        dir[..w.n].copy_from_slice(&w.b[..w.n]);
        w.n
    };
    let path = |file: &str, out: &mut [u8; 240]| -> usize {
        let n = (dl + file.len()).min(out.len());
        out[..dl].copy_from_slice(&dir[..dl]);
        out[dl..n].copy_from_slice(&file.as_bytes()[..n - dl]);
        n
    };
    let read = |fs: &mut gs::fs::Fs<'_>, file: &str, buf: &mut [u8]| -> Result<usize, Text> {
        let mut p = [0u8; 240];
        let n = path(file, &mut p);
        fs.read_into(core::str::from_utf8(&p[..n]).unwrap_or(""), buf).map_err(|e| match e {
            gs::Error::NotFound => text(format_args!("{} is missing", file)),
            e => text(format_args!("{} could not be read: {:?}", file, e)),
        })
    };

    // The signature, over the manifest exactly as kept.
    let mut m = [0u8; MANIFEST_MAX];
    let ml = read(&mut fs, "manifest.toml", &mut m)?;
    let mut s = [0u8; MSIG_MAX];
    let sl = read(&mut fs, "manifest.sig", &mut s)?;
    let sig_text = core::str::from_utf8(&s[..sl]).map_err(|_| text(format_args!("manifest.sig is not text")))?;
    let sig = ManifestSignature::parse(sig_text).and_then(|g| g.verify(&m[..ml]).map(|()| g))
        .map_err(|e| text(format_args!("the signature does not verify over manifest.toml ({:?})", e)))?;

    // Where it came from, and the key that signed it then.
    let mut o = [0u8; ORIGIN_MAX];
    let ol = read(&mut fs, "origin", &mut o)?;
    let origin = core::str::from_utf8(&o[..ol]).unwrap_or("");
    let field = |k: &str| origin.lines().find_map(|l| l.strip_prefix(k)).unwrap_or("");
    let mut key = Text { b: [0; 160], n: 0 };
    let _ = core::fmt::Write::write_fmt(&mut key, format_args!("{}", sig.key));
    if field("signed-by ") != key.as_str() {
        return Err(text(format_args!("signed by another key than the one recorded when it was fetched")));
    }
    let registry = field("registry ").split(' ').next().unwrap_or("");
    if registry.is_empty() || registry.contains('/') || registry.contains("..") {
        return Err(text(format_args!("origin names no registry it came from")));
    }

    // The key's standing in this machine's trust record for that registry.
    {
        let mut ns = [0u8; NAMESPACES_MAX];
        let mut p = Text { b: [0; 160], n: 0 };
        let _ = core::fmt::Write::write_fmt(&mut p, format_args!("{}{}/namespaces.toml", TRUST, registry));
        let nl = match fs.read_into(p.as_str(), &mut ns) {
            Ok(n) => n,
            Err(gs::Error::NotFound) => return Err(text(format_args!("this machine holds no trust record for `{}`", registry))),
            Err(e) => return Err(text(format_args!("the trust record for `{}` could not be read: {:?}", registry, e))),
        };
        let nt = core::str::from_utf8(&ns[..nl]).map_err(|_| text(format_args!("the trust record is not text")))?;
        let names = Namespaces::parse(nt).map_err(|e| text(format_args!("the trust record does not parse ({:?})", e)))?;
        // A release from a key retired since counts only if it was published before; a copy fetched before the
        // date was recorded has none to show, so it needs the key active.
        let published = field("published ");
        let ok = match names.status(publisher, &sig.key) {
            Some(KeyStatus::Active) => true,
            Some(KeyStatus::Retired(at)) => !published.is_empty() && published < at,
            Some(KeyStatus::Revoked(_)) => return Err(text(format_args!("its signing key has been REVOKED by the registry"))),
            None => false,
        };
        if !ok { return Err(text(format_args!("its signing key is not one this machine trusts for {}", publisher))); }
    }

    // The manifest names this package, and the contract it binds.
    let mtext = core::str::from_utf8(&m[..ml]).map_err(|_| text(format_args!("manifest.toml is not text")))?;
    let mut arts = Artifacts::new();
    let man = meta::manifest(mtext, &mut arts).map_err(|b| text(format_args!("manifest.toml line {}: {}", b.line, b.why)))?.manifest;
    let mut ver = Text { b: [0; 160], n: 0 };
    let _ = core::fmt::Write::write_fmt(&mut ver, format_args!("{}", man.version));
    if man.name.publisher() != publisher || man.name.name() != name || ver.as_str() != version {
        return Err(text(format_args!("the manifest names another package or version than its folder")));
    }
    let mut c = [0u8; CONTRACT_MAX];
    let cl = read(&mut fs, godspeed_package::CONTRACT_FILE, &mut c)?;
    if Sha256Digest::of(&c[..cl]) != man.contract {
        return Err(text(format_args!("contract.toml is not the contract the manifest names")));
    }
    let ctext = core::str::from_utf8(&c[..cl]).map_err(|_| text(format_args!("contract.toml is not text")))?;
    let (cname, cver, requested) = contract(ctext)?;
    if cname != name || cver != version {
        return Err(text(format_args!("the contract names another service or version than the package")));
    }

    // The binary, every byte of it, handed on as it is hashed.
    let want = match arch { Some(a) => a, None => field("arch ") };
    let Some(artifact) = man.artifacts.iter().find(|a| a.arch.as_str() == want).copied() else {
        return Err(text(format_args!("the package has no binary for {}", if want.is_empty() { "the architecture recorded" } else { want })));
    };
    let file = artifact.arch.artifact_file();
    let mut verifier = ArtifactVerifier::new(artifact);
    let mut chunk = [0u8; gs::fs::IO_CHUNK];
    let mut off = 0u64;
    let mut bp = [0u8; 240];
    let bn = path(file, &mut bp);
    let bin = core::str::from_utf8(&bp[..bn]).unwrap_or("");
    loop {
        let got = fs.read_at(bin, off, &mut chunk).map_err(|e| match e {
            gs::Error::NotFound => text(format_args!("{} is missing", file)),
            e => text(format_args!("{} could not be read: {:?}", file, e)),
        })?;
        if got == 0 { break; }
        verifier.update(&chunk[..got]).map_err(|_| text(format_args!("{} is longer than the manifest says", file)))?;
        if !image(off, &chunk[..got]) { return Err(text(format_args!("{} is larger than this loader holds", file))); }
        off += got as u64;
    }
    verifier.finish().map_err(|_| text(format_args!("{} is not the size or digest the manifest names", file)))?;
    Ok(Checked { arch: artifact.arch.as_str(), size: artifact.size, contract: requested, kind: man.kind })
}
