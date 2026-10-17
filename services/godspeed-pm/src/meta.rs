// SPDX-License-Identifier: GPL-2.0-only
//! A package's index and manifest, read without a heap into the shared crate's own types
//! (`godspeed_package::PackageIndex`, `Manifest`), so every rule that crate checks is checked here by the
//! same code: the reading is ours (`toml_lines.rs`), the meaning is the crate's.
//!
//! The text is only ever read AFTER it is verified - the index against the signed snapshot, the manifest
//! against the index's digest - so a parse failure is a registry fault, never an attacker's input reaching
//! further than this reader.

use godspeed_package::{Arch, Artifact, Kind, Manifest, PackageIndex, PackageName, Release, Sha256Digest, Version};

use crate::toml_lines::{lines, Line, Val};

/// The most releases of one package read. A package with more is refused by name rather than cut short:
/// the newest might be the one not read. (The index buffer, `packages.rs`, holds about 40.)
pub const MAX_RELEASES: usize = 40;

/// Why a document was refused, and on which line (0 when it is the document as a whole).
#[derive(Clone, Copy, Debug)]
pub struct Bad {
    pub line: usize,
    pub why: &'static str,
}

fn bad(line: usize, why: &'static str) -> Bad {
    Bad { line, why }
}

/// A string that must be exactly as written: no escape in it.
fn plain<'a>(line: usize, v: Val<'a>) -> Result<&'a str, Bad> {
    match v {
        Val::Str(s) => Ok(s),
        Val::Escaped(_) => Err(bad(line, "a field that must match exactly holds an escape")),
        Val::Int(_) | Val::Bool(_) | Val::Strings(_) => Err(bad(line, "a number, boolean or array where a string belongs")),
    }
}

fn int(line: usize, v: Val<'_>) -> Result<u64, Bad> {
    match v {
        Val::Int(n) => Ok(n),
        _ => Err(bad(line, "a string where a number belongs")),
    }
}

fn digest(line: usize, v: Val<'_>) -> Result<Sha256Digest, Bad> {
    plain(line, v)?.parse().map_err(|_| bad(line, "a digest is not sha256:<64 hex digits>"))
}

fn version(line: usize, v: Val<'_>) -> Result<Version, Bad> {
    plain(line, v)?.parse().map_err(|_| bad(line, "a version is not MAJOR.MINOR.PATCH"))
}

const PLACEHOLDER: Release<'static> = Release {
    version: Version::new(0, 0, 0),
    manifest: Sha256Digest([0; 32]),
    published: "",
    published_by: "",
};

/// Space for an index's releases, filled by [`index`].
pub struct Releases<'a> {
    pub items: [Release<'a>; MAX_RELEASES],
    pub len: usize,
}

impl<'a> Releases<'a> {
    pub fn new() -> Releases<'a> {
        Releases { items: [PLACEHOLDER; MAX_RELEASES], len: 0 }
    }
}

/// Read a package index. The crate's `validate` runs on the result; a problem it reports is refused here.
pub fn index<'a>(text: &'a str, rel: &'a mut Releases<'a>) -> Result<PackageIndex<'a>, Bad> {
    let mut format: Option<u64> = None;
    let mut name: Option<PackageName<'a>> = None;
    // One release at a time, completed when the next `[[release]]` begins or the text ends.
    let mut cur: Option<(usize, Option<Version>, Option<Sha256Digest>, Option<&'a str>, Option<&'a str>)> = None;
    let finish = |cur: &mut Option<(usize, Option<Version>, Option<Sha256Digest>, Option<&'a str>, Option<&'a str>)>,
                      rel: &mut Releases<'a>| -> Result<(), Bad> {
        if let Some((at, v, m, p, b)) = cur.take() {
            let (Some(version), Some(manifest), Some(published), Some(published_by)) = (v, m, p, b) else {
                return Err(bad(at, "a release lacks its version, manifest, published or published_by"));
            };
            if rel.len == MAX_RELEASES {
                return Err(bad(at, "more releases than this machine reads (40)"));
            }
            rel.items[rel.len] = Release { version, manifest, published, published_by };
            rel.len += 1;
        }
        Ok(())
    };
    for (n, l) in lines(text) {
        match l.map_err(|why| bad(n, why))? {
            Line::ArrayTable("release") => {
                finish(&mut cur, rel)?;
                cur = Some((n, None, None, None, None));
            }
            Line::ArrayTable(_) | Line::Table(_) => return Err(bad(n, "a table an index does not have")),
            Line::Pair(k, v) => match (&mut cur, k) {
                (None, "format") => format = Some(int(n, v)?),
                (None, "name") => {
                    name = Some(PackageName::parse(plain(n, v)?).map_err(|_| bad(n, "the name is not publisher/name"))?)
                }
                (Some(c), "version") => c.1 = Some(version(n, v)?),
                (Some(c), "manifest") => c.2 = Some(digest(n, v)?),
                (Some(c), "published") => c.3 = Some(plain(n, v)?),
                (Some(c), "published_by") => c.4 = Some(plain(n, v)?),
                _ => return Err(bad(n, "a key an index does not have")),
            },
        }
    }
    finish(&mut cur, rel)?;
    let format = format.ok_or(bad(0, "the index has no format"))?;
    let name = name.ok_or(bad(0, "the index has no name"))?;
    if rel.len == 0 {
        return Err(bad(0, "the index lists no release"));
    }
    let idx = PackageIndex {
        format: u32::try_from(format).map_err(|_| bad(0, "the format number is too large"))?,
        name,
        releases: &rel.items[..rel.len],
    };
    let mut first: Option<&'static str> = None;
    if !idx.validate(|p| { first.get_or_insert(p.field); }) {
        return Err(bad(0, match first {
            Some("format") => "an index format this machine does not read",
            Some("release.version") => "a version is listed twice",
            Some("release.published") => "a publication time is malformed or out of order",
            Some("release.published_by") => "a publishing account is not a valid name",
            _ => "the index breaks the format's rules",
        }));
    }
    Ok(idx)
}

/// Space for a manifest's artifacts, filled by [`manifest`].
pub struct Artifacts {
    pub items: [Artifact; 4],
    pub len: usize,
}

impl Artifacts {
    pub fn new() -> Artifacts {
        let a = Artifact { arch: Arch::X86_64, size: 0, digest: Sha256Digest([0; 32]) };
        Artifacts { items: [a; 4], len: 0 }
    }
}

/// A manifest read, with the two strings it may carry as written (escapes and all) for showing.
pub struct Read<'a> {
    pub manifest: Manifest<'a>,
}

/// Read a manifest. The crate's `validate` runs on the result; a problem it reports is refused here.
pub fn manifest<'a>(text: &'a str, arts: &'a mut Artifacts) -> Result<Read<'a>, Bad> {
    #[derive(PartialEq)]
    enum In { Top, Package, Os, Contract, Artifact }
    let mut at = In::Top;
    let (mut format, mut name, mut ver, mut kind, mut desc, mut lic, mut os, mut contract) =
        (None, None, None, None, None, None, None, None);
    let mut cur: Option<(usize, Option<Arch>, Option<u64>, Option<Sha256Digest>)> = None;
    let finish = |cur: &mut Option<(usize, Option<Arch>, Option<u64>, Option<Sha256Digest>)>, arts: &mut Artifacts| -> Result<(), Bad> {
        if let Some((n, a, s, d)) = cur.take() {
            let (Some(arch), Some(size), Some(digest)) = (a, s, d) else {
                return Err(bad(n, "an artifact lacks its arch, size or digest"));
            };
            if arts.len == arts.items.len() {
                return Err(bad(n, "more artifacts than there are architectures"));
            }
            arts.items[arts.len] = Artifact { arch, size, digest };
            arts.len += 1;
        }
        Ok(())
    };
    for (n, l) in lines(text) {
        match l.map_err(|why| bad(n, why))? {
            Line::Table(t) => {
                finish(&mut cur, arts)?;
                at = match t {
                    "package" => In::Package,
                    "os" => In::Os,
                    "contract" => In::Contract,
                    _ => return Err(bad(n, "a table a manifest does not have")),
                };
            }
            Line::ArrayTable("artifact") => {
                finish(&mut cur, arts)?;
                at = In::Artifact;
                cur = Some((n, None, None, None));
            }
            Line::ArrayTable(_) => return Err(bad(n, "a table a manifest does not have")),
            Line::Pair(k, v) => match (&at, k) {
                (In::Top, "format") => format = Some(int(n, v)?),
                (In::Package, "name") => {
                    name = Some(PackageName::parse(plain(n, v)?).map_err(|_| bad(n, "the name is not publisher/name"))?)
                }
                (In::Package, "version") => ver = Some(version(n, v)?),
                (In::Package, "kind") => kind = Some(match plain(n, v)? {
                    "service" => Kind::Service,
                    "utility" => Kind::Utility,
                    "app" => Kind::App,
                    "driver" => Kind::Driver,
                    _ => return Err(bad(n, "a package kind this machine does not know")),
                }),
                (In::Package, "description") => desc = Some(match v { Val::Str(s) | Val::Escaped(s) => s, Val::Int(_) | Val::Bool(_) | Val::Strings(_) => return Err(bad(n, "a number or array where a string belongs")) }),
                (In::Package, "license") => lic = Some(match v { Val::Str(s) | Val::Escaped(s) => s, Val::Int(_) | Val::Bool(_) | Val::Strings(_) => return Err(bad(n, "a number or array where a string belongs")) }),
                (In::Os, "release") => os = Some(version(n, v)?),
                (In::Contract, "digest") => contract = Some(digest(n, v)?),
                (In::Artifact, "arch") => {
                    let a: Arch = plain(n, v)?.parse().map_err(|_| bad(n, "an architecture this format does not name"))?;
                    if let Some(c) = &mut cur { c.1 = Some(a); }
                }
                (In::Artifact, "size") => { let s = int(n, v)?; if let Some(c) = &mut cur { c.2 = Some(s); } }
                (In::Artifact, "digest") => { let d = digest(n, v)?; if let Some(c) = &mut cur { c.3 = Some(d); } }
                _ => return Err(bad(n, "a key a manifest does not have")),
            },
        }
    }
    finish(&mut cur, arts)?;
    let m = Manifest {
        format: u32::try_from(format.ok_or(bad(0, "the manifest has no format"))?).map_err(|_| bad(0, "the format number is too large"))?,
        name: name.ok_or(bad(0, "the manifest has no package name"))?,
        version: ver.ok_or(bad(0, "the manifest has no version"))?,
        kind: kind.ok_or(bad(0, "the manifest has no kind"))?,
        description: desc,
        license: lic,
        os_release: os.ok_or(bad(0, "the manifest has no [os] release"))?,
        contract: contract.ok_or(bad(0, "the manifest has no [contract] digest"))?,
        artifacts: &arts.items[..arts.len],
    };
    let mut first: Option<&'static str> = None;
    if !m.validate(|p| { first.get_or_insert(p.field); }) {
        return Err(bad(0, match first {
            Some("format") => "a manifest format this machine does not read",
            Some("artifact") => "the manifest names no binary",
            Some("artifact.arch") => "the binaries repeat an architecture, or are out of order",
            Some("artifact.size") => "a binary is empty or larger than any package may be",
            Some("package.description") | Some("package.license") => "the description or licence holds a control character or is too long",
            _ => "the manifest breaks the format's rules",
        }));
    }
    Ok(Read { manifest: m })
}
