//! An abstract model of a filesystem, written from the SPEC and knowing nothing about GSFS.
//!
//! **THE ONE RULE THAT MAKES THIS WORTH BUILDING** (`docs/gsfs-carnage.md` §3.2): this must not
//! reuse GSFS allocation, traversal, rename or recovery logic. A model that shares the
//! implementation reproduces its bugs and agrees with them, which is worse than no model because it
//! produces a green tick.
//!
//! So nothing here is derived from `services/fs`. The rules below are read off `utilities/*.md` -
//! what a person is told each command does - and encoded as a `BTreeMap` of paths to contents. That
//! is the point of the exercise: every other storage suite we have tests GSFS against assertions
//! written *about GSFS*, so if a belief about what `rename` should do is wrong, all of them agree
//! with the bug. This disagrees exactly where that belief is wrong.
//!
//! Host-side, so `BTreeMap`/`String` are fine - §26.6.1 governs what runs on the machine, not what
//! runs in `osdev`.
//!
//! **What is compared, and what deliberately is not.** The model predicts only **Ok versus Err**,
//! never the error VARIANT. The shell has four (`FileNotFound`, `Denied`, `AssertFailed`,
//! `Unknown`) and which one a refusal picks is shell implementation detail; a model that predicted
//! it would be coupled to the thing it is supposed to be independent of. The strong signal is the
//! second comparison: at the end of a run every file's bytes and every directory's name set are
//! read back off the disk and must match the model exactly.
//!
//! No interruption here. Under injected faults the comparison stops being equality and becomes
//! MEMBERSHIP of the permitted-outcome set (§2 of the carnage doc); that is the follow-up, and it
//! needs this to exist first.

use std::collections::{BTreeMap, BTreeSet};

/// Longest name GSFS stores. Taken from the on-disk format the spec publishes, not from the code -
/// it is a property of the FORMAT, which is a fact a model is allowed to know.
pub const NAME_MAX: usize = 38;

/// One operation, in the vocabulary the shell actually offers.
#[derive(Clone, Debug)]
pub enum Op {
    Mkdir(String),
    /// `write <path> <content>`
    Write(String, String),
    Delete(String),
    /// `rename <path> <new NAME>` - a NAME, not a path. The spec is explicit and it is the kind of
    /// thing a model gets right only by reading it.
    Rename(String, String),
    Seal(String),
    Read(String),
    List(String),
}

impl Op {
    /// The exact shell command line.
    pub fn line(&self) -> String {
        match self {
            Op::Mkdir(p) => format!("mkdir {p}"),
            Op::Write(p, c) => format!("write {p} {c}"),
            Op::Delete(p) => format!("delete {p}"),
            Op::Rename(p, n) => format!("rename {p} {n}"),
            // `yes` skips the [y/N] confirm. That is not a test shortcut - it is the escape the
            // command documents for exactly this, because a confirm READS THE CONSOLE and nothing
            // automated can answer one. The warning still prints either way.
            Op::Seal(p) => format!("seal {p} yes"),
            Op::Read(p) => format!("read {p}"),
            Op::List(p) => format!("dir {p}"),
        }
    }
}

#[derive(Clone, Debug, Default)]
struct FileState {
    content: String,
    sealed: bool,
}

/// The whole filesystem, as the spec describes it.
#[derive(Clone, Debug)]
pub struct Model {
    dirs: BTreeSet<String>,
    files: BTreeMap<String, FileState>,
}

fn parent_of(path: &str) -> Option<String> {
    let p = path.trim_end_matches('/');
    let i = p.rfind('/')?;
    Some(if i == 0 { "/".to_string() } else { p[..i].to_string() })
}

fn name_of(path: &str) -> &str {
    let p = path.trim_end_matches('/');
    match p.rfind('/') {
        Some(i) => &p[i + 1..],
        None => p,
    }
}

fn valid_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= NAME_MAX
        && n != "."
        && n != ".."
        && n.bytes().all(|b| (0x20..0x7f).contains(&b) && b != b'/')
}

impl Model {
    pub fn new() -> Self {
        let mut dirs = BTreeSet::new();
        dirs.insert("/".to_string());
        Model { dirs, files: BTreeMap::new() }
    }

    pub fn is_dir(&self, p: &str) -> bool { self.dirs.contains(p) }
    pub fn is_file(&self, p: &str) -> bool { self.files.contains_key(p) }
    fn exists(&self, p: &str) -> bool { self.is_dir(p) || self.is_file(p) }

    /// Entries directly inside `dir`, by name.
    pub fn children(&self, dir: &str) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        for d in &self.dirs {
            if d != "/" && parent_of(d).as_deref() == Some(dir) {
                out.insert(name_of(d).to_string());
            }
        }
        for f in self.files.keys() {
            if parent_of(f).as_deref() == Some(dir) {
                out.insert(name_of(f).to_string());
            }
        }
        out
    }

    pub fn content(&self, p: &str) -> Option<&str> {
        self.files.get(p).map(|f| f.content.as_str())
    }

    pub fn all_files(&self) -> Vec<String> { self.files.keys().cloned().collect() }
    pub fn all_dirs(&self) -> Vec<String> { self.dirs.iter().cloned().collect() }

    /// Apply `op`, returning whether the SPEC says it should succeed.
    ///
    /// Every rule here is a sentence somebody could read in `utilities/`. Where the spec is silent
    /// the model refuses rather than guesses, because a model that permits more than the spec says
    /// will call a real refusal a bug.
    pub fn apply(&mut self, op: &Op) -> bool {
        match op {
            // `mkdir <path>` - the parent must exist and be a directory; the name must be free.
            Op::Mkdir(p) => {
                let Some(par) = parent_of(p) else { return false };
                if !self.is_dir(&par) || !valid_name(name_of(p)) || self.exists(p) {
                    return false;
                }
                self.dirs.insert(p.clone());
                true
            }
            // `write <path> <content>` - creates or overwrites. A SEALED file's content can never
            // change (`utilities/50_seal.md`), and a directory is not a file.
            Op::Write(p, c) => {
                let Some(par) = parent_of(p) else { return false };
                if !self.is_dir(&par) || !valid_name(name_of(p)) || self.is_dir(p) {
                    return false;
                }
                if self.files.get(p).map(|f| f.sealed).unwrap_or(false) {
                    return false;
                }
                self.files.insert(p.clone(), FileState { content: c.clone(), sealed: false });
                true
            }
            // `delete <path>` - without the word `recursive` a non-empty directory is refused.
            Op::Delete(p) => {
                if self.is_file(p) {
                    self.files.remove(p);
                    return true;
                }
                if self.is_dir(p) && p != "/" {
                    if !self.children(p).is_empty() {
                        return false;
                    }
                    self.dirs.remove(p);
                    return true;
                }
                false
            }
            // `rename <path> <name>` - a NAME. The entry stays in its own directory.
            Op::Rename(p, n) => {
                if !valid_name(n) || !self.exists(p) || p == "/" {
                    return false;
                }
                let Some(par) = parent_of(p) else { return false };
                let dest = if par == "/" { format!("/{n}") } else { format!("{par}/{n}") };
                if self.exists(&dest) {
                    return false;
                }
                if let Some(f) = self.files.remove(p) {
                    self.files.insert(dest, f);
                    return true;
                }
                // A directory rename carries everything beneath it.
                let old = p.clone();
                let moved: Vec<String> =
                    self.dirs.iter().filter(|d| **d == old || d.starts_with(&format!("{old}/"))).cloned().collect();
                for d in &moved {
                    self.dirs.remove(d);
                    self.dirs.insert(dest.clone() + &d[old.len()..]);
                }
                let touched: Vec<String> =
                    self.files.keys().filter(|f| f.starts_with(&format!("{old}/"))).cloned().collect();
                for f in touched {
                    let st = self.files.remove(&f).unwrap();
                    self.files.insert(dest.clone() + &f[old.len()..], st);
                }
                true
            }
            // `seal <path>` - files only, permanent, and IDEMPOTENT: sealing an already-sealed file
            // succeeds and changes nothing (`utilities/50_seal.md`).
            //
            // THIS MODEL ORIGINALLY PREDICTED A REFUSAL, and that disagreement is the first thing
            // this oracle ever found. It was not a bug in `fs` - it was a rule that lived only in
            // the code, in a one-line comment, while the spec this model is written from said
            // nothing at all. The spec says it now. Left as a comment because the whole value of a
            // model is that it is written from the document, so a place where the document was
            // silent is worth marking on both sides.
            Op::Seal(p) => {
                match self.files.get_mut(p) {
                    Some(f) => { f.sealed = true; true }
                    None => false,
                }
            }
            Op::Read(p) => self.is_file(p),
            Op::List(p) => self.is_dir(p),
        }
    }
}

/// A seeded generator, so a failure is reproducible from its seed alone.
///
/// A plain 64-bit LCG: the sequence has to be identical on every host and every run, which rules
/// out anything that consults the system, and nothing here needs statistical quality - it needs to
/// be REPEATABLE, because "preserve the seed" is the first line of §5 of the carnage doc.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self { Rng(seed | 1) }
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 11
    }
    pub fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T { &xs[(self.next() as usize) % xs.len()] }
}

/// The path universe: deliberately SMALL, so collisions happen.
///
/// A generator over a wide namespace mostly creates files that never meet, and the interesting
/// defects live exactly where two operations touch the same entry - rename onto an existing name,
/// write into a path that is a directory, delete a directory that still has children. Nine paths
/// across two levels produce those constantly.
pub fn universe(root: &str) -> Vec<String> {
    let mut v = vec![];
    for n in ["f1", "f2", "f3", "d1", "d2"] {
        v.push(format!("{root}/{n}"));
    }
    for d in ["d1", "d2"] {
        for n in ["g1", "g2"] {
            v.push(format!("{root}/{d}/{n}"));
        }
    }
    v
}

/// One random operation over `universe`.
pub fn gen_op(rng: &mut Rng, paths: &[String]) -> Op {
    let p = rng.pick(paths).clone();
    match rng.next() % 10 {
        0 | 1 => Op::Mkdir(p),
        2 | 3 | 4 => {
            // Content is short and alphanumeric: the aim is to catch a filesystem losing or mixing
            // bytes, not to test the shell's tokeniser, and a space would make `write` take only
            // the first word.
            let c = format!("c{}", rng.next() % 100000);
            Op::Write(p, c)
        }
        5 => Op::Delete(p),
        6 => {
            let n = format!("r{}", rng.next() % 40);
            Op::Rename(p, n)
        }
        7 => Op::Seal(p),
        8 => Op::Read(p),
        _ => Op::List(p),
    }
}
