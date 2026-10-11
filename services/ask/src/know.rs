// SPDX-License-Identifier: GPL-2.0-only
//! What `ask` knows, with no system attached: how a question is read, and how the answers its sources
//! gave become an answer whose every line is a FACT, an INFERENCE or an UNKNOWN (`docs/ask-design.md`).
//!
//! **Separate so it can be tested on the host.** `main.rs` asks the services; this decides what their
//! answers mean, and it names nothing outside `core`, so it compiles on its own:
//!
//! ```text
//! rustc --edition 2021 --test services/ask/src/know.rs -o build/know_test && build/know_test
//! ```
//!
//! The tests are the design's section 8: the vocabulary (what is understood, what is ambiguous, what is
//! refused), and fixtures of source replies with what each answer MUST say and what it must NOT.

use core::fmt::Write;

// ---------------------------------------------------------------------------------------------
// Understanding the question (design section 4). Deterministic, versioned, small, no model.
// ---------------------------------------------------------------------------------------------

/// The vocabulary's version. Bumped when a word is added or a reading changes; the tests below pin
/// every question an earlier version understood, so a question understood stays understood.
pub const VOCABULARY: u32 = 1;

/// The question kinds of v0.1 (design section 3): the four specific ones, and the general one they sit
/// beside - "why did <service> <event>".
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// Is the system healthy?
    Health,
    /// What failed?
    Failed,
    /// Why did the WiFi restart (or drop, or disconnect)? An ALIAS: the WiFi is whichever radio driver
    /// is running, and the radio's own status is asked as well.
    Wifi,
    /// Is the filesystem healthy?
    Fs,
    /// Why did (did, has) this service <event>? The SUBJECT is not vocabulary: it is an index into the
    /// names the machine itself knows (`understand`'s `known`), so a new service needs no new word.
    Service(usize),
    /// What can be asked? ("what to say?", "what can I ask?") Answered with the question forms and
    /// suggestions taken from this machine as it is - never a question about something it lacks.
    Guide,
    /// An Easter egg (operator, 2026-10-11). Still honest: its verdict says what it is - `A JOKE` - so a
    /// script reading verdicts cannot take one for an answer, and every line is true or is a joke.
    Egg(Egg),
}

/// The Easter eggs. Deterministic and fixed in the program; a joke rotates with the uptime second.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Egg {
    Joke,
    Meaning,
    Sudo,
    RmRf,
    Alive,
    Who,
}

impl Kind {
    /// The kind in words; a service is named from `known`.
    pub fn word<'k>(self, known: &[&'k str]) -> &'k str {
        match self {
            Kind::Health => "system health",
            Kind::Failed => "what failed",
            Kind::Wifi => "the WiFi",
            Kind::Fs => "the filesystem",
            Kind::Service(i) => known.get(i).copied().unwrap_or("a service"),
            Kind::Guide => "what can be asked",
            Kind::Egg(_) => "an Easter egg",
        }
    }
}

/// Which WiFi event a question names. Kept apart on purpose (design 4: restart, reboot, reset,
/// disconnect and crash each keep their own meaning), because the answer depends on which happened.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Event {
    Restart,
    /// Failed. Not the same word as a crash, and neither is a restart: a service restarted by a
    /// deliberate `kill` restarted and did not fail.
    Fail,
    Crash,
    Stop,
    Disconnect,
    Reset,
    /// The question named no event ("is the wifi ok?").
    Unnamed,
}

impl Event {
    pub fn word(self) -> &'static str {
        match self {
            Event::Restart => "restart",
            Event::Fail => "fail",
            Event::Crash => "crash",
            Event::Stop => "stop",
            Event::Disconnect => "drop the link",
            Event::Reset => "reset",
            Event::Unnamed => "",
        }
    }
}

/// How a question was read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Reading {
    One(Kind, Event),
    /// "What is X?" - a term to define, at `at..at + len` in the question as it was asked.
    Define { at: usize, len: usize },
    /// Two kinds fit; the answer lists both and asks which (never picks one to have an answer).
    Two(Kind, Kind),
    /// A request to DO something, which `ask` cannot; the verb is the first word.
    Action,
    Empty,
    NotUnderstood,
}

/// The longest question read (design section 7).
pub const QUESTION_MAX: usize = 512;
const WORDS_MAX: usize = 48;

/// Normalise and split: ASCII lower case, anything that is not a letter or a digit is a space, and a
/// few spellings are made one (`wi-fi`, `wi fi`, `wireless` name one subsystem). Returns the words.
fn words<'b>(q: &[u8], buf: &'b mut [u8; QUESTION_MAX], out: &mut [(usize, usize); WORDS_MAX]) -> usize {
    let n = q.len().min(QUESTION_MAX);
    for i in 0..n {
        let c = q[i].to_ascii_lowercase();
        buf[i] = if c.is_ascii_alphanumeric() { c } else { b' ' };
    }
    let mut count = 0usize;
    let mut i = 0usize;
    while i < n && count < WORDS_MAX {
        while i < n && buf[i] == b' ' { i += 1; }
        let s = i;
        while i < n && buf[i] != b' ' { i += 1; }
        if i > s { out[count] = (s, i); count += 1; }
    }
    // `wi fi` is one word.
    let mut w = 0usize;
    let mut r = 0usize;
    while r < count {
        let (s, e) = out[r];
        if &buf[s..e] == b"wi" && r + 1 < count && &buf[out[r + 1].0..out[r + 1].1] == b"fi" {
            buf[s] = b'w'; buf[s + 1] = b'i'; buf[s + 2] = b'f'; buf[s + 3] = b'i';
            out[w] = (s, s + 4);
            r += 2;
        } else {
            out[w] = (s, e);
            r += 1;
        }
        w += 1;
    }
    w
}

fn any(word: &[u8], set: &[&str]) -> bool {
    set.iter().any(|s| s.as_bytes() == word)
}

const QUESTION_WORDS: &[&str] = &["why", "what", "is", "are", "did", "does", "has", "have", "was", "were",
                                  "how", "which", "when", "who", "anything", "any", "tell", "show"];
/// A first word that asks for an action. `ask` reads; it never does.
const ACTION_WORDS: &[&str] = &["restart", "reboot", "kill", "stop", "start", "fix", "repair", "format",
                                "delete", "reset", "join", "connect", "disconnect", "flash", "scrub", "run"];
const WIFI_WORDS: &[&str] = &["wifi", "wireless", "wlan", "radio", "network"];
const FS_WORDS: &[&str] = &["filesystem", "fs", "disk", "drive", "storage", "volume", "gsfs"];
const FS_PAIR: (&str, &str) = ("file", "system");
const HEALTH_WORDS: &[&str] = &["healthy", "health", "ok", "okay", "fine", "working", "well", "alright",
                                "good", "sick", "wrong"];
const FAILED_WORDS: &[&str] = &["failed", "fail", "failure", "failures", "crashed", "crash", "crashes",
                                "died", "dead", "broke", "broken", "restarted", "restart", "restarts"];
const SYSTEM_WORDS: &[&str] = &["system", "machine", "computer", "everything", "os", "godspeed", "box"];

/// Where a known service name occurs in the question. A name is matched as words, normalised as the
/// question is (`nic-driver` is `nic driver`), and the LONGEST match wins, so `nic-driver` is not read as
/// a bare `driver`. Returns the best match and, if a second, different service is also named, that one.
fn find_service(buf: &[u8], ws: &[(usize, usize)], n: usize, known: &[&str]) -> (Option<usize>, Option<usize>) {
    let mut best: Option<(usize, usize)> = None; // (index into known, words it spans)
    let mut second: Option<usize> = None;
    for (ki, name) in known.iter().enumerate() {
        let mut nb = [0u8; 64];
        let mut nw = [(0usize, 0usize); 8];
        let nl = name.len().min(64);
        nb[..nl].copy_from_slice(&name.as_bytes()[..nl]);
        let mut cnt = 0usize;
        let mut i = 0usize;
        while i < nl && cnt < 8 {
            while i < nl && !nb[i].is_ascii_alphanumeric() { i += 1; }
            let st = i;
            while i < nl && nb[i].is_ascii_alphanumeric() { nb[i] = nb[i].to_ascii_lowercase(); i += 1; }
            if i > st { nw[cnt] = (st, i); cnt += 1; }
        }
        if cnt == 0 { continue; }
        let hit = (0..n).any(|s| s + cnt <= n && (0..cnt).all(|k| buf[ws[s + k].0..ws[s + k].1] == nb[nw[k].0..nw[k].1]));
        if !hit { continue; }
        match best {
            None => best = Some((ki, cnt)),
            Some((bi, bc)) => {
                // One mention read two ways (`wifi-usb` contains `wifi`) is not two services.
                let same = known[bi].contains(name) || name.contains(known[bi]);
                if cnt > bc {
                    if !same { second = Some(bi); }
                    best = Some((ki, cnt));
                } else if !same && second.is_none() {
                    second = Some(ki);
                }
            }
        }
    }
    (best.map(|b| b.0), second)
}

/// Read a question about this machine. `known` is the names of its services - the task table's and the
/// supervisor's - so a question may name any of them without the vocabulary listing it.
pub fn understand(q: &[u8], known: &[&str]) -> Reading {
    let mut buf = [0u8; QUESTION_MAX];
    let mut ws = [(0usize, 0usize); WORDS_MAX];
    let n = words(q, &mut buf, &mut ws);
    if n == 0 {
        return Reading::Empty;
    }
    let w = |i: usize| &buf[ws[i].0..ws[i].1];

    // THE EASTER EGGS, before anything else reads their words as something they are not ("sudo" is no
    // action word here, and "what is godspeed" is not the health question).
    if any(w(0), &["sudo"]) { return Reading::One(Kind::Egg(Egg::Sudo), Event::Unnamed); }
    if w(0) == b"rm" && (0..n).any(|i| w(i) == b"rf" || w(i) == b"r") { return Reading::One(Kind::Egg(Egg::RmRf), Event::Unnamed); }
    let has_e = |set: &[&str]| (0..n).any(|i| any(w(i), set));
    if has_e(&["joke", "jokes", "funny", "laugh"]) { return Reading::One(Kind::Egg(Egg::Joke), Event::Unnamed); }
    if has_e(&["meaning"]) && has_e(&["life", "universe", "everything"]) { return Reading::One(Kind::Egg(Egg::Meaning), Event::Unnamed); }
    if has_e(&["alive", "sentient", "conscious"]) && has_e(&["you"]) { return Reading::One(Kind::Egg(Egg::Alive), Event::Unnamed); }
    if (has_e(&["who", "what"]) && has_e(&["you"]) && has_e(&["are"]) && n <= 4)
        || (has_e(&["what", "who"]) && has_e(&["godspeed"]) && n <= 4) {
        return Reading::One(Kind::Egg(Egg::Who), Event::Unnamed);
    }

    // A TERM TO DEFINE: "what is X?", "what's X", "what does X mean / stand for?", "define X" - one word,
    // and not one of ask's own question words ("what is wrong?" is still the health question).
    {
        let mut idx = [0usize; WORDS_MAX];
        let mut m = 0usize;
        for i in 0..n {
            if !any(w(i), &["the", "a", "an", "acronym", "abbreviation", "term", "word"]) { idx[m] = i; m += 1; }
        }
        let ww = |k: usize| w(idx[k]);
        // The term is a SPAN of up to three words, because a hyphenated one splits (`amd-vi` is
        // `amd vi`): (first, last) indices into `idx`.
        let span = if m >= 3 && m <= 5 && ww(0) == b"what" && any(ww(1), &["is", "s", "are"]) { Some((2, m - 1)) }
            else if m >= 2 && m <= 4 && any(ww(0), &["whats", "define", "explain"]) { Some((1, m - 1)) }
            else if m >= 4 && m <= 7 && ww(0) == b"what" && ww(1) == b"does" {
                (3..m).find(|&k| any(ww(k), &["mean", "stand"])).map(|k| (2, k - 1))
            } else { None };
        if let Some((f0, f1)) = span {
            let ours = (f0..=f1).any(|k| { let x = ww(k);
                any(x, HEALTH_WORDS) || any(x, FAILED_WORDS) || any(x, SYSTEM_WORDS) || any(x, QUESTION_WORDS) });
            if f1 >= f0 && !ours {
                let at = ws[idx[f0]].0;
                return Reading::Define { at, len: ws[idx[f1]].1 - at };
            }
        }
    }

    // WHAT CAN BE ASKED - "what to say?", "what can I ask?", "what questions?", "help" - before anything
    // else, because "what" and "ask" would otherwise be read as part of some other question.
    let has = |set: &[&str]| (0..n).any(|i| any(w(i), set));
    if (n == 1 && any(w(0), &["help", "suggestions", "examples"]))
        || (has(&["what", "which"]) && has(&["say", "ask", "questions", "question"]))
        || (has(&["help"]) && has(&["me", "ask", "questions"])) {
        return Reading::One(Kind::Guide, Event::Unnamed);
    }

    // A request to DO something: an action word first, and no question word anywhere before it.
    if any(w(0), ACTION_WORDS) {
        return Reading::Action;
    }

    let (mut wifi, mut fs, mut health, mut failed, mut system) = (false, false, false, false, false);
    let mut event = Event::Unnamed;
    let mut questioned = false;
    for i in 0..n {
        let x = w(i);
        if any(x, QUESTION_WORDS) { questioned = true; }
        if any(x, WIFI_WORDS) { wifi = true; }
        if any(x, FS_WORDS) { fs = true; }
        if x == FS_PAIR.0.as_bytes() && i + 1 < n && w(i + 1) == FS_PAIR.1.as_bytes() { fs = true; }
        if any(x, HEALTH_WORDS) { health = true; }
        if any(x, FAILED_WORDS) { failed = true; }
        if any(x, SYSTEM_WORDS) { system = true; }
        // The event the question names. First one wins; none of them is folded into another.
        if event == Event::Unnamed {
            event = if any(x, &["restart", "restarted", "restarts", "restarting"]) { Event::Restart }
                else if any(x, &["fail", "failed", "fails", "failing", "failure"]) { Event::Fail }
                else if any(x, &["crash", "crashed", "crashes", "died", "dead", "die"]) { Event::Crash }
                else if any(x, &["stop", "stopped", "stops", "exited", "exit"]) { Event::Stop }
                else if any(x, &["disconnect", "disconnected", "drop", "dropped", "drops", "lost"]) { Event::Disconnect }
                else if any(x, &["reset", "resets"]) { Event::Reset }
                else { Event::Unnamed };
        }
    }
    let _ = questioned;
    // A NAMED SERVICE WITH AN EVENT is the general question - "why did fs restart", "did the xhci driver
    // crash" - and it takes precedence over the specific kinds, because naming the service is the more
    // precise question: "why did fs restart" asks about the `fs` service, not the filesystem's health.
    // Without an event, a service word that is also a subsystem word ("is fs ok") stays the subsystem
    // question. The WiFi words remain an alias (below): "the wifi" names no service.
    let (svc, other) = find_service(&buf, &ws, n, known);
    if let Some(s) = svc {
        if let Some(o) = other {
            return Reading::Two(Kind::Service(s), Kind::Service(o));
        }
        if event != Event::Unnamed || !(wifi || fs || health || failed) {
            return Reading::One(Kind::Service(s), event);
        }
    }
    // A SUBSYSTEM WORD DECIDES THE KIND: "is the filesystem healthy" asks about the filesystem, and
    // "why did the wifi fail" about the WiFi - neither is the system-wide question. Two subsystems
    // named at once is the ambiguity this must not settle by itself.
    match (wifi, fs) {
        (true, true) => return Reading::Two(Kind::Wifi, Kind::Fs),
        (true, false) => return Reading::One(Kind::Wifi, event),
        (false, true) => return Reading::One(Kind::Fs, Event::Unnamed),
        (false, false) => {}
    }
    match (health || system, failed) {
        (true, true) if health => Reading::Two(Kind::Health, Kind::Failed),
        (_, true) => Reading::One(Kind::Failed, Event::Unnamed),
        (true, false) if health => Reading::One(Kind::Health, Event::Unnamed),
        _ => Reading::NotUnderstood,
    }
}

// ---------------------------------------------------------------------------------------------
// The answer (design section 5). Every line is one of three, and a verdict heads it.
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Label {
    /// The one-line answer.
    Verdict,
    /// A source said it, or recorded it.
    Fact,
    /// Concluded from facts by a written rule; the rule is named.
    Inference,
    /// Not established by anything available.
    Unknown,
    /// Something to say that is none of those: a clarification, what `ask` can answer.
    Note,
}

impl Label {
    pub fn word(self) -> &'static str {
        match self {
            Label::Verdict => "verdict",
            Label::Fact => "fact",
            Label::Inference => "inference",
            Label::Unknown => "unknown",
            Label::Note => "note",
        }
    }
}

/// How the question went, for the shell's result: an answered question is a success whatever the
/// answer says; the rest are failures, and this says which (design section 2).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Answered,
    /// Answered, but a source it needed did not answer, so the answer names a hole.
    SourceSilent,
    NotUnderstood,
    Ambiguous,
    Action,
    Empty,
}

impl Status {
    pub fn word(self) -> &'static str {
        match self {
            Status::Answered => "answered",
            Status::SourceSilent => "source-silent",
            Status::NotUnderstood => "not-understood",
            Status::Ambiguous => "ambiguous",
            Status::Action => "action",
            Status::Empty => "empty",
        }
    }
}

pub const TEXT_MAX: usize = 160;
/// Lines in one answer. 48 x 160 bytes is under the 16 KiB output bound (design section 7).
pub const LINES_MAX: usize = 48;

#[derive(Clone, Copy)]
pub struct Line {
    pub label: Label,
    /// Which source a fact came from, or `-`.
    pub source: &'static str,
    /// For an inference, the rule that made it; otherwise `-`.
    pub rule: &'static str,
    text: [u8; TEXT_MAX],
    len: usize,
}

impl Line {
    const fn empty() -> Line {
        Line { label: Label::Note, source: "-", rule: "-", text: [0; TEXT_MAX], len: 0 }
    }
    pub fn text(&self) -> &[u8] {
        &self.text[..self.len]
    }
}

pub struct Answer {
    lines: [Line; LINES_MAX],
    n: usize,
    pub status: Status,
    /// Lines that did not fit. Said in the answer itself rather than dropped (design section 7).
    pub cut: usize,
}

struct TextBuf<'a> { buf: &'a mut [u8; TEXT_MAX], len: &'a mut usize }
impl Write for TextBuf<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for &b in s.as_bytes() {
            if *self.len < TEXT_MAX { self.buf[*self.len] = b; *self.len += 1; }
        }
        Ok(())
    }
}

impl Answer {
    pub fn new() -> Answer {
        Answer { lines: [Line::empty(); LINES_MAX], n: 0, status: Status::Answered, cut: 0 }
    }
    pub fn lines(&self) -> &[Line] {
        &self.lines[..self.n]
    }
    fn put(&mut self, label: Label, source: &'static str, rule: &'static str, args: core::fmt::Arguments) {
        // The last slot is kept for the line that says lines were cut.
        if self.n >= LINES_MAX - 1 {
            self.cut += 1;
            return;
        }
        let l = &mut self.lines[self.n];
        l.label = label;
        l.source = source;
        l.rule = rule;
        l.len = 0;
        let _ = TextBuf { buf: &mut l.text, len: &mut l.len }.write_fmt(args);
        self.n += 1;
    }
    pub fn verdict(&mut self, args: core::fmt::Arguments) { self.put(Label::Verdict, "-", "-", args) }
    pub fn fact(&mut self, src: &'static str, args: core::fmt::Arguments) { self.put(Label::Fact, src, "-", args) }
    pub fn infer(&mut self, rule: &'static str, args: core::fmt::Arguments) { self.put(Label::Inference, "-", rule, args) }
    pub fn unknown(&mut self, args: core::fmt::Arguments) { self.put(Label::Unknown, "-", "-", args) }
    pub fn note(&mut self, args: core::fmt::Arguments) { self.put(Label::Note, "-", "-", args) }
    /// Close the answer: say if lines were cut. Call once, last.
    pub fn finish(&mut self) {
        if self.cut > 0 {
            let cut = self.cut;
            let l = &mut self.lines[self.n];
            *l = Line::empty();
            l.label = Label::Note;
            let _ = TextBuf { buf: &mut l.text, len: &mut l.len }
                .write_fmt(format_args!("this answer is PARTIAL - {} more line(s) did not fit", cut));
            self.n += 1;
        }
    }
    pub fn has(&self, label: Label, needle: &str) -> bool {
        self.lines().iter().any(|l| l.label == label && contains(l.text(), needle.as_bytes()))
    }
    pub fn any_text(&self, needle: &str) -> bool {
        self.lines().iter().any(|l| contains(l.text(), needle.as_bytes()))
    }
}

fn contains(h: &[u8], n: &[u8]) -> bool {
    n.is_empty() || h.windows(n.len()).any(|w| w == n)
}

// ---------------------------------------------------------------------------------------------
// What the sources said. Filled by `main.rs` from real requests; by the tests from fixtures.
// ---------------------------------------------------------------------------------------------

/// A source's answer: what it said, or that it did not answer, or that it is not on this machine.
/// "Did not answer" and "not here" are different facts and are never folded together.
#[derive(Clone, Copy)]
pub enum Got<T> {
    Yes(T),
    /// The source exists and did not answer in time.
    Silent,
    /// Nothing on this machine answers for it.
    Absent,
}

/// One live task, from the task table.
#[derive(Clone, Copy)]
pub struct Task<'a> {
    pub name: &'a str,
    /// Times this service was respawned since boot.
    pub restarts: u64,
    /// Seconds since this instance started.
    pub up_secs: u64,
}

/// The supervisor's record of what happened to the services it drives (`supcmd::EVENTS`).
#[derive(Clone, Copy)]
pub struct Ev<'a> {
    pub at: u32,
    pub code: u8,
    pub name: &'a str,
}

#[derive(Clone, Copy)]
pub struct Events<'a> {
    /// When the supervisor began this record, in seconds of uptime.
    pub since: u32,
    /// Every event it noted; more than `list.len()` means the oldest were overwritten.
    pub recorded: u32,
    pub list: &'a [Ev<'a>],
}

/// The supervisor's event codes (`supcmd::EV_*`), restated so this file names nothing outside `core`.
/// `main.rs` asserts at compile time that they match.
pub const EV_RESTARTED: u8 = 1;
pub const EV_RESTART_FAILED: u8 = 2;
pub const EV_ATTACHED: u8 = 3;
pub const EV_DETACHED: u8 = 4;
pub const EV_SWEPT: u8 = 5;
pub const EV_START_FAILED: u8 = 6;

/// `fs`'s drive report (`OP_DRIVES_INFO`).
#[derive(Clone, Copy)]
pub struct Drive<'a> {
    /// 512-byte sectors; zero means no storage device at all.
    pub capacity: u64,
    pub mounted: bool,
    pub total_blocks: u64,
    pub used_blocks: u64,
    pub label: &'a str,
}

/// A radio driver's status (`godspeed_wifi::wire::OP_STATUS`), reduced to what the answers use.
#[derive(Clone, Copy)]
pub enum Radio<'a> {
    /// Up; associated or not, and to which network name.
    Up { associated: bool, ssid: &'a str, joined_secs: u32 },
    /// Switched off by `wifi radio off`.
    Off,
    /// Powered down by `wifi radio off hard`.
    PoweredOff,
    /// Down, and why (`wire::DOWN_*`, 0 = the driver did not say).
    Down(u8),
}

pub const DOWN_TRAPPED: u8 = 1;
pub const DOWN_BRINGUP: u8 = 2;
pub const DOWN_NO_RADIO: u8 = 3;
pub const DOWN_NOT_BUILT: u8 = 4;

#[derive(Clone, Copy)]
pub struct RadioFact<'a> {
    /// The driver service, `wifi-driver` or `wifi-usb`.
    pub driver: &'a str,
    pub status: Got<Radio<'a>>,
}

/// The storage chain below `fs`, each link asked the same read-only question - "how big is your disk?" -
/// in its own framing. Gathered only when the filesystem is the question and `fs` has no usable disk.
#[derive(Clone, Copy)]
pub struct Chain<'a> {
    /// `block-driver`'s answer, in sectors; zero is it saying there is no medium.
    pub block: Got<u64>,
    /// The USB host `block-driver` holds a send capability to - the link its disk is behind - and that
    /// host's own answer. `None` when `block-driver` holds no capability to a USB host (a disk it drives
    /// itself, as AHCI on a PC), which is a fact too.
    pub host: Option<(&'a str, Got<u64>)>,
    /// Whether `block-driver`'s capabilities could be read at all. When they could not, which link it
    /// sits on is UNKNOWN rather than "none".
    pub host_read: bool,
}

pub struct Facts<'a> {
    /// When these facts were taken, in seconds of uptime. Every answer carries it (design 5: two
    /// invocations are two observations).
    pub now: u64,
    /// A fine-grained moment (the kernel's tick counter), used only to choose which joke: two questions
    /// in the same second would otherwise get the same one.
    pub tick: u64,
    /// The live tasks. Always available: `ask` holds `INTROSPECT` for it.
    pub tasks: &'a [Task<'a>],
    /// The services the supervisor expects to be running right now (`supcmd::MANAGED`).
    pub expected: Got<&'a [&'a str]>,
    pub events: Got<Events<'a>>,
    pub drive: Got<Drive<'a>>,
    /// One entry per radio driver that is running; empty when none is.
    pub radios: &'a [RadioFact<'a>],
    /// The storage chain, when it was asked.
    pub chain: Option<Chain<'a>>,
}

impl<'a> Facts<'a> {
    fn task(&self, name: &str) -> Option<&Task<'a>> {
        self.tasks.iter().find(|t| t.name == name)
    }
}

/// Walk `fs` -> `block-driver` -> its USB host and say where storage stops: the DEEPEST link that is not
/// running or did not answer, or - when every link answers - that the last one has no disk.
///
/// Deepest, not first: a link that waits on the one below it goes silent BECAUSE that one is. In QEMU
/// `block-driver` and `xhci` were both silent and `block-driver`'s own log said it was waiting 2 s on
/// `xhci`; naming the first silent link blamed the one that was only waiting.
fn chain(c: Chain, a: &mut Answer) -> &'static str {
    let mut stop: Option<&'static str> = None;
    match c.block {
        Got::Yes(0) => a.fact("block-driver", format_args!("`block-driver` answers that it has no medium (0 sectors)")),
        Got::Yes(n) => a.fact("block-driver", format_args!("`block-driver` answers {} sectors ({} MiB)", n, n / 2048)),
        Got::Silent => { a.fact("block-driver", format_args!("`block-driver` is running and did not answer in time")); stop = Some("block-driver"); }
        Got::Absent => { a.fact("task table", format_args!("`block-driver` is not running")); stop = Some("block-driver"); }
    }
    match (c.host_read, c.host) {
        (false, _) => a.unknown(format_args!("which USB host `block-driver`'s disk is behind could not be read")),
        (true, None) => a.fact("task table", format_args!("`block-driver` holds no capability to a USB host - it drives its disk itself")),
        (true, Some((name, ans))) => {
            a.fact("task table", format_args!("`block-driver` reaches its disk through `{}` (it holds a capability to it)", name));
            match ans {
                Got::Yes(0) => a.fact(host_src(name), format_args!("`{}` answers that no disk is attached to it", name)),
                Got::Yes(n) => a.fact(host_src(name), format_args!("`{}` answers {} sectors", name, n)),
                Got::Silent | Got::Absent => {
                    if matches!(ans, Got::Silent) {
                        a.fact(host_src(name), format_args!("`{}` is running and did not answer in time", name));
                    } else {
                        a.fact("task table", format_args!("`{}` is not running", name));
                    }
                    // Deeper than `block-driver`, so it is where storage stops - and a silent
                    // `block-driver` above it may only be waiting on it.
                    if stop == Some("block-driver") && matches!(c.block, Got::Silent) {
                        a.unknown(format_args!("whether `block-driver` is at fault too cannot be told: it may only be waiting on `{}`", name));
                    }
                    stop = Some(host_src(name));
                }
            }
        }
    }
    match stop {
        Some(link) => {
            a.infer("deepest-silent-link", format_args!("storage stops at the deepest link that is not running or did not answer: `{}`", link));
            match link {
                "block-driver" => "STORAGE STOPS AT block-driver",
                "xhci" => "STORAGE STOPS AT xhci",
                "dwc2" => "STORAGE STOPS AT dwc2",
                "ehci" => "STORAGE STOPS AT ehci",
                _ => "STORAGE STOPS BELOW fs",
            }
        }
        None => {
            a.infer("every-link-answers", format_args!("every link answers, and none has a disk: nothing is attached, rather than anything being broken"));
            "NO DISK ATTACHED"
        }
    }
}

/// A source word for a USB host's answer: static, and only the hosts this system has.
fn host_src(name: &str) -> &'static str {
    match name { "xhci" => "xhci", "dwc2" => "dwc2", "ehci" => "ehci", _ => "usb host" }
}

/// The supervisor's record keeps the newest 32 and starts empty when the supervisor restarts.
fn say_record_bounds(a: &mut Answer, ev: &Events) {
    a.fact("supervisor", format_args!("its record of driver events began at uptime {}s (it starts empty when the supervisor restarts)", ev.since));
    let kept = ev.list.len() as u32;
    if ev.recorded > kept {
        a.fact("supervisor", format_args!("{} earlier event(s) were overwritten - the record keeps the newest {}", ev.recorded - kept, kept));
    }
}

fn down_words(reason: u8) -> &'static str {
    match reason {
        DOWN_TRAPPED => "its firmware was loaded and trapped at start",
        DOWN_BRINGUP => "its bring-up stopped before the firmware started",
        DOWN_NO_RADIO => "no working radio answered on its bus",
        DOWN_NOT_BUILT => "its bring-up stopped before a station interface existed",
        _ => "it did not say why",
    }
}

// ---------------------------------------------------------------------------------------------
// The four assessments (design section 3). Each states its facts, then its inferences by named rule,
// then what it cannot know - and only then the verdict, which is placed FIRST in the answer.
// ---------------------------------------------------------------------------------------------

/// The severity ladder for a health verdict.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Sev { Healthy, Degraded, Unhealthy }

/// Answer a question that was read as `kind`. The answer starts with a verdict line.
pub fn assess(kind: Kind, event: Event, known: &[&str], f: &Facts, a: &mut Answer) {
    // The verdict is decided last but shown first: reserve its slot.
    let verdict_at = a.n;
    a.verdict(format_args!(""));
    let mut vbuf = [0u8; TEXT_MAX];
    let mut vlen = 0usize;
    {
        let mut v = TextBuf { buf: &mut vbuf, len: &mut vlen };
        match kind {
            Kind::Health => { let _ = v.write_str(health(f, a)); }
            Kind::Failed => { let _ = v.write_str(failed(f, a)); }
            Kind::Wifi => { let _ = v.write_str(wifi(event, f, a)); }
            Kind::Fs => { let _ = v.write_str(fs(f, a)); }
            Kind::Service(i) => service(known.get(i).copied().unwrap_or("?"), event, f, a, &mut v),
            Kind::Guide => { let _ = v.write_str(guide(f, a)); }
            Kind::Egg(e) => { let _ = v.write_str(egg(e, f, a)); }
        }
    }
    a.fact("ask", format_args!("taken at uptime {}s; another question is another observation", f.now));
    let l = &mut a.lines[verdict_at];
    l.text = vbuf;
    l.len = vlen;
    a.finish();
}

/// Why did (did, has) `name` <event>? From the same sources as the specific kinds: the task table, the
/// supervisor's record, and the supervisor's list of what it keeps running.
fn service(name: &str, event: Event, f: &Facts, a: &mut Answer, v: &mut TextBuf) {
    let asked = event.word();
    if !asked.is_empty() {
        a.note(format_args!("you asked whether `{}` did {}; a restart, a failure, a crash and a stop are kept apart, and a deliberate kill is none of them", name, asked));
    }
    let task = f.task(name);
    let expected = match f.expected { Got::Yes(names) => names.iter().any(|n| *n == name), _ => false };
    match task {
        Some(t) if t.restarts > 0 => a.fact("task table", format_args!("`{}` has restarted {} time(s) since boot; this instance has been up {}s", name, t.restarts, t.up_secs)),
        Some(t) => a.fact("task table", format_args!("`{}` is running and has not restarted since boot (up {}s)", name, t.up_secs)),
        None => a.fact("task table", format_args!("`{}` is not running now", name)),
    }
    if task.is_none() && expected {
        a.fact("supervisor", format_args!("the supervisor keeps `{}` running, so it is down when it should be up", name));
    }
    let mut recorded = 0u32;
    match f.events {
        Got::Yes(ev) => {
            for e in ev.list.iter().filter(|e| e.name == name) {
                let what = match e.code {
                    EV_RESTARTED => "died and was restarted",
                    EV_RESTART_FAILED => "died, and its restart FAILED",
                    EV_SWEPT => "was found dead by the supervisor's sweep and restarted",
                    EV_ATTACHED => "was started because its device was reported attached",
                    EV_DETACHED => "was stopped because its device was reported gone",
                    EV_START_FAILED => "was wanted for a device that arrived, and could not be started",
                    _ => continue,
                };
                recorded += 1;
                a.fact("supervisor", format_args!("at uptime {}s `{}` {}", e.at, name, what));
            }
            if recorded == 0 {
                a.fact("supervisor", format_args!("its record holds no event for `{}` (it records the services that drive a device)", name));
            }
            say_record_bounds(a, &ev);
        }
        Got::Silent => hole(a, "the supervisor"),
        Got::Absent => {}
    }
    let restarts = task.map(|t| t.restarts).unwrap_or(0);
    if restarts > 0 || recorded > 0 || task.is_none() {
        a.unknown(format_args!("why `{}` died is not recorded anywhere this can read", name));
        if matches!(event, Event::Fail | Event::Crash) {
            a.unknown(format_args!("so whether it FAILED cannot be said: a deliberate `kill` restarts a service too, and the record does not say which"));
        }
    }
    let _ = match (task, restarts) {
        (None, _) if expected => write!(v, "`{}` IS DOWN NOW", name),
        (None, _) => write!(v, "`{}` IS NOT RUNNING", name),
        // The event asked about decides the words: a running service that never restarted has not
        // stopped either, and "has not restarted" would answer a different question.
        (Some(_), 0) if matches!(event, Event::Stop | Event::Crash | Event::Fail) => write!(v, "`{}` IS RUNNING AND HAS NOT RESTARTED SINCE BOOT", name),
        (Some(_), 0) => write!(v, "`{}` HAS NOT RESTARTED SINCE BOOT", name),
        (Some(_), r) => write!(v, "`{}` HAS RESTARTED {} TIME(S) - WHY IS NOT RECORDED", name, r),
    };
}

fn hole(a: &mut Answer, what: &str) {
    a.status = Status::SourceSilent;
    a.unknown(format_args!("{} did not answer, so what it knows is a hole in this answer", what));
}

fn health(f: &Facts, a: &mut Answer) -> &'static str {
    let mut worst = Sev::Healthy;
    let mut holes = 0u32;

    a.fact("task table", format_args!("{} task(s) are running", f.tasks.len()));
    match f.expected {
        Got::Yes(names) => {
            let missing = names.iter().filter(|n| f.task(n).is_none()).count();
            for n in names.iter().filter(|n| f.task(n).is_none()) {
                a.fact("supervisor", format_args!("`{}` is a service the supervisor runs, and it is not running now", n));
            }
            if missing > 0 {
                a.infer("required-missing", format_args!("a service the supervisor keeps running is down, so the system is UNHEALTHY"));
                worst = worst.max(Sev::Unhealthy);
            } else {
                a.fact("supervisor", format_args!("all {} service(s) the supervisor keeps running are running", names.len()));
            }
        }
        Got::Silent => { holes += 1; hole(a, "the supervisor"); }
        Got::Absent => { holes += 1; a.unknown(format_args!("which services must be running could not be asked")); }
    }
    let mut restarted = 0u32;
    for t in f.tasks.iter().filter(|t| t.restarts > 0) {
        restarted += 1;
        a.fact("task table", format_args!("`{}` has restarted {} time(s) since boot; this instance has been up {}s", t.name, t.restarts, t.up_secs));
    }
    if restarted > 0 {
        a.infer("restarted-live", format_args!("a service that restarted and is running again works now, but something made it restart, so the system is DEGRADED"));
        worst = worst.max(Sev::Degraded);
        a.unknown(format_args!("why each restart happened is not recorded anywhere this can read; a deliberate `kill` restarts a service too"));
    }
    match f.drive {
        Got::Yes(d) if d.capacity == 0 => {
            a.fact("fs", format_args!("no storage device is attached"));
            a.infer("no-disk-not-a-fault", format_args!("a machine with no disk is not unhealthy for that alone - nothing was there to fail"));
        }
        Got::Yes(d) if !d.mounted => {
            a.fact("fs", format_args!("a storage device is attached and nothing is mounted on it"));
            a.infer("storage-unmounted", format_args!("storage is present but not usable, so the system is DEGRADED"));
            worst = worst.max(Sev::Degraded);
        }
        Got::Yes(_) => a.fact("fs", format_args!("the filesystem is mounted and answering")),
        // SILENCE IS A HOLE, NEVER A VERDICT. `fs` that is slow to answer is not `fs` that cannot: in QEMU
        // its drive report waits on `block-driver`, which waits 2 s on `xhci`, and the first cut of this
        // called that UNHEALTHY - the convincing answer the evidence does not support (design 5).
        Got::Silent => { holes += 1; hole(a, "`fs`"); }
        Got::Absent => { holes += 1; a.unknown(format_args!("storage could not be asked - `fs` is not running")); }
    }
    for r in f.radios {
        match r.status {
            // NO RADIO ANSWERED is two different machines - one with none fitted (QEMU, a board without the
            // part) and one whose radio is broken - and nothing here can tell them apart.
            Got::Yes(Radio::Down(DOWN_NO_RADIO)) => {
                a.fact(r.driver_src(), format_args!("`{}` says no working radio answered on its bus", r.driver));
                a.unknown(format_args!("whether this machine has a radio fitted cannot be told from here, so a missing one is not called a fault"));
            }
            Got::Yes(Radio::Down(why)) => {
                a.fact(r.driver_src(), format_args!("`{}` says the radio is down: {}", r.driver, down_words(why)));
                a.infer("radio-down", format_args!("a radio is present and not working, so the system is DEGRADED"));
                worst = worst.max(Sev::Degraded);
            }
            Got::Yes(Radio::Up { associated, .. }) => a.fact(r.driver_src(), format_args!("`{}` says the radio is up, {}", r.driver, if associated { "and joined to a network" } else { "and not joined to any network" })),
            Got::Yes(Radio::Off) | Got::Yes(Radio::PoweredOff) => a.fact(r.driver_src(), format_args!("`{}` says the radio is switched off", r.driver)),
            Got::Silent => { holes += 1; hole(a, r.driver); }
            Got::Absent => {}
        }
    }
    match worst {
        Sev::Unhealthy => "UNHEALTHY",
        Sev::Degraded => "DEGRADED",
        // A machine with no recorded error is not therefore healthy (design 3.1): every required check
        // must have RUN for this word to be said.
        Sev::Healthy if holes > 0 => {
            a.infer("coverage", format_args!("nothing that answered found a fault, but not everything answered, so this cannot say HEALTHY"));
            "UNKNOWN"
        }
        Sev::Healthy => "HEALTHY",
    }
}

impl RadioFact<'_> {
    fn driver_src(&self) -> &'static str {
        // Sources are static words; the two radio drivers are the only names here.
        if self.driver == "wifi-usb" { "wifi-usb" } else { "wifi-driver" }
    }
}

fn failed(f: &Facts, a: &mut Answer) -> &'static str {
    let mut seen = 0u32;
    match f.events {
        Got::Yes(ev) => {
            for e in ev.list {
                let what = match e.code {
                    EV_RESTARTED => "died and was restarted",
                    EV_RESTART_FAILED => "died, and its restart FAILED",
                    EV_SWEPT => "was found dead by the supervisor's sweep and restarted",
                    EV_START_FAILED => "was wanted for a device that arrived, and could not be started",
                    // Arrivals and departures are not failures.
                    EV_ATTACHED | EV_DETACHED => continue,
                    _ => continue,
                };
                seen += 1;
                a.fact("supervisor", format_args!("at uptime {}s `{}` {}", e.at, e.name, what));
            }
            say_record_bounds(a, &ev);
        }
        Got::Silent => hole(a, "the supervisor"),
        Got::Absent => a.unknown(format_args!("the supervisor's record of driver events could not be asked")),
    }
    let mut restarted = 0u32;
    for t in f.tasks.iter().filter(|t| t.restarts > 0) {
        restarted += 1;
        a.fact("task table", format_args!("`{}` has restarted {} time(s) since boot and is running now", t.name, t.restarts));
    }
    let mut down = 0u32;
    if let Got::Yes(names) = f.expected {
        for n in names.iter().filter(|n| f.task(n).is_none()) {
            down += 1;
            a.fact("supervisor", format_args!("`{}` should be running and is not", n));
        }
    }
    if seen + restarted + down > 0 {
        a.unknown(format_args!("why any of these died is not recorded anywhere this can read"));
        a.unknown(format_args!("a restart is not proof of a crash: a deliberate `kill` restarts a service too, and the record does not say which"));
    }
    if down > 0 { "SOMETHING IS DOWN NOW" }
    else if seen + restarted > 0 { "RESTARTS RECORDED - everything that restarted is running again" }
    else if matches!(f.events, Got::Yes(_)) {
        a.infer("nothing-recorded", format_args!("no restart is recorded and no service restarted since boot; that is what the record shows, not proof nothing went wrong"));
        "NOTHING RECORDED"
    } else { "UNKNOWN" }
}

fn wifi(event: Event, f: &Facts, a: &mut Answer) -> &'static str {
    if f.radios.is_empty() {
        a.fact("task table", format_args!("no radio driver (`wifi-driver`, `wifi-usb`) is running on this machine"));
        if let Got::Yes(ev) = f.events {
            if ev.list.iter().any(|e| e.name == "wifi-driver" || e.name == "wifi-usb") {
                a.fact("supervisor", format_args!("the supervisor did record radio driver events; `ask \"what failed?\"` lists them"));
            }
        }
        a.infer("no-radio", format_args!("with no radio driver there is no WiFi to have restarted"));
        return "NO RADIO";
    }
    let asked = event.word();
    if !asked.is_empty() {
        a.note(format_args!("you asked why it did {} - that is kept apart from a driver restart, a power cycle, a firmware fault and a dropped link", asked));
    }
    let mut verdict = "NO RESTART RECORDED";
    for r in f.radios {
        let restarts = f.task(r.driver).map(|t| t.restarts).unwrap_or(0);
        if restarts > 0 {
            a.fact("task table", format_args!("`{}` has restarted {} time(s) since boot", r.driver, restarts));
            verdict = "THE RADIO DRIVER RESTARTED";
        } else {
            a.fact("task table", format_args!("`{}` has not restarted since boot (restart count 0)", r.driver));
        }
        if let Got::Yes(ev) = f.events {
            for e in ev.list.iter().filter(|e| e.name == r.driver) {
                let what = match e.code {
                    EV_RESTARTED => "died and was restarted",
                    EV_RESTART_FAILED => "died and its restart failed",
                    EV_SWEPT => "was found dead and restarted",
                    EV_ATTACHED => "was started because its device was reported attached",
                    EV_DETACHED => "was stopped because its device was reported gone",
                    _ => continue,
                };
                a.fact("supervisor", format_args!("at uptime {}s `{}` {}", e.at, e.name, what));
            }
        }
        match r.status {
            Got::Yes(Radio::Down(why)) => {
                a.fact(r.driver_src(), format_args!("`{}` says the radio is down now: {}", r.driver, down_words(why)));
                if restarts > 0 {
                    // WHAT CAME BEFORE, NOT WHY (design 3.3).
                    a.note(format_args!("that fault is what the radio shows now; nothing records that it caused the restart"));
                }
            }
            Got::Yes(Radio::Up { associated: true, ssid, joined_secs }) =>
                a.fact(r.driver_src(), format_args!("`{}` says the radio is up and joined to `{}` for {}s", r.driver, ssid, joined_secs)),
            Got::Yes(Radio::Up { associated: false, .. }) => {
                a.fact(r.driver_src(), format_args!("`{}` says the radio is up and not joined to any network", r.driver));
                if event == Event::Disconnect || restarts == 0 {
                    a.unknown(format_args!("when or why the link dropped is not recorded anywhere this can read"));
                }
            }
            Got::Yes(Radio::Off) => a.fact(r.driver_src(), format_args!("`{}` says the radio was switched off (`wifi radio off`)", r.driver)),
            Got::Yes(Radio::PoweredOff) => a.fact(r.driver_src(), format_args!("`{}` says the chip is powered down (`wifi radio off hard`)", r.driver)),
            Got::Silent => hole(a, r.driver),
            Got::Absent => {}
        }
        if restarts > 0 {
            a.unknown(format_args!("why `{}` died is not recorded anywhere this can read", r.driver));
        }
    }
    verdict
}

fn fs(f: &Facts, a: &mut Answer) -> &'static str {
    match f.drive {
        Got::Silent => {
            hole(a, "`fs`");
            if let Some(c) = f.chain { return chain(c, a); }
            "UNKNOWN - fs did not answer"
        }
        Got::Absent => {
            a.fact("task table", format_args!("`fs` is not running"));
            "NO FILESYSTEM SERVICE"
        }
        Got::Yes(d) if d.capacity == 0 => {
            a.fact("fs", format_args!("`fs` has no storage device: `block-driver` reported no capacity"));
            if let Some(c) = f.chain { return chain(c, a); }
            "NO DISK"
        }
        Got::Yes(d) if !d.mounted => {
            a.fact("fs", format_args!("a {} MiB device is attached and nothing is mounted on it (not formatted, or not mountable)", d.capacity / 2048));
            "NOT MOUNTED"
        }
        Got::Yes(d) => {
            let pct = if d.total_blocks == 0 { 0 } else { d.used_blocks * 100 / d.total_blocks };
            a.fact("fs", format_args!("mounted: `{}`, {} MiB, {}% used", if d.label.is_empty() { "-" } else { d.label }, d.capacity / 2048, pct));
            if let Some(t) = f.task("fs") {
                if t.restarts > 0 {
                    a.fact("task table", format_args!("`fs` has restarted {} time(s) since boot; each restart re-mounts through the journal", t.restarts));
                }
            }
            a.infer("mounted-answering", format_args!("it mounted and answers, which is what NOW looks like; it says nothing about blocks nobody has read"));
            a.unknown(format_args!("whether a journal was replayed at the last mount is not reported by any request `fs` answers"));
            a.unknown(format_args!("whether it is mounted read-only is not reported by any request `fs` answers"));
            a.note(format_args!("`drives scrub` reads every block's CRC and changes nothing; it is offered, not run, because it reads the whole volume"));
            "MOUNTED - NOT VERIFIED"
        }
    }
}

/// "What to say?" - the forms `ask` understands, each with an example, then suggestions from THIS machine:
/// the services that have restarted, the ones that should be running and are not, the radio and the
/// storage where there are any. Every suggestion is a question `ask` would answer here, so none of them
/// asks about something the machine does not have.
fn guide(f: &Facts, a: &mut Answer) -> &'static str {
    a.note(format_args!("ask \"is my system healthy?\"  - a verdict, and what it rests on"));
    a.note(format_args!("ask \"what failed?\"  - what restarted, what failed to, what is down now"));
    a.note(format_args!("ask \"why did <service> <event>?\"  - a service by name; restart, fail, crash or stop"));
    a.note(format_args!("ask evidence \"<question>\"  - the same answer, every line labelled and its source named"));
    // Suggestions from what is true now, most-restarted first, at most three.
    let mut picked: [Option<&Task>; 3] = [None; 3];
    for t in f.tasks.iter().filter(|t| t.restarts > 0) {
        let slot = picked.iter().position(|p| p.map_or(true, |q| t.restarts > q.restarts));
        if let Some(i) = slot {
            let mut j = picked.len() - 1;
            while j > i { picked[j] = picked[j - 1]; j -= 1; }
            picked[i] = Some(t);
        }
    }
    let mut suggested = 0u32;
    for t in picked.iter().flatten() {
        suggested += 1;
        a.note(format_args!("try: ask \"why did {} restart?\"  - it has restarted {} time(s)", t.name, t.restarts));
    }
    if let Got::Yes(names) = f.expected {
        for n in names.iter().filter(|n| f.task(n).is_none()).take(2) {
            suggested += 1;
            a.note(format_args!("try: ask \"why did {} stop?\"  - it should be running and is not", n));
        }
    }
    if !f.radios.is_empty() {
        suggested += 1;
        a.note(format_args!("try: ask \"why did the wifi restart?\"  - a radio driver is running"));
    }
    if f.task("fs").is_some() {
        suggested += 1;
        a.note(format_args!("try: ask \"is the filesystem healthy?\""));
    }
    if suggested == 0 {
        a.note(format_args!("nothing here has restarted, so there is no restart to ask about yet"));
    }
    "YOU CAN ASK"
}

/// Godspeed jokes. Each is about the system, and the punchline is true.
const JOKES: &[&str] = &[
    "How many Godspeed services does it take to change a light bulb? None. The bulb holds no capability, so nobody may touch it.",
    "Knock knock. - Who's there? - EndpointDead. - EndpointDead who? - Reacquire me by name and ask again.",
    "Why is the kernel so calm? It has exactly six jobs, and it refuses a seventh.",
    // MISCIS, said "misses": Memory isolation, IPC, Scheduling, Capabilities, Interrupts, SMP routing
    // (CLAUDE.md 4.3) - the six the kernel does, and the reason it does nothing else.
    "Why is the Godspeed kernel so small? Anything that isn't MISCIS, it misses on purpose.",
    "A UNIX process walks into a bar and says it is root. The barman asks to see its capability.",
    "Why did the service restart? It lost its capability to cope. The supervisor brought it back anyway.",
    "I would tell you a UDP joke, but I cannot promise net-stack delivers it.",
    "Why do Godspeed services never gossip? Every message needs a SEND right, and the kernel checks.",
];

/// An Easter egg. The verdict says what it is; the lines are a joke or are true.
fn egg(e: Egg, f: &Facts, a: &mut Answer) -> &'static str {
    match e {
        Egg::Joke => {
            let j = JOKES[(f.tick as usize) % JOKES.len()];
            a.note(format_args!("{}", j));
            a.note(format_args!("(a joke, not an observation - ask again for another)"));
            "A JOKE"
        }
        Egg::Meaning => {
            a.note(format_args!("42."));
            a.unknown(format_args!("why is not recorded anywhere this can read"));
            "42"
        }
        Egg::Sudo => {
            a.note(format_args!("There is no sudo. Authority here is a capability you hold, not a user you become (CLAUDE.md 3.1)."));
            a.note(format_args!("And I hold nothing that changes the system: I only read."));
            "NO SUDO HERE"
        }
        Egg::RmRf => {
            a.note(format_args!("Nothing here holds the ambient authority to do that, and I hold none at all (CLAUDE.md Appendix D.4)."));
            "NOT HERE"
        }
        Egg::Alive => {
            a.note(format_args!("No. I am a program started for one question, and I end when this answer does."));
            a.note(format_args!("`observe now` after this prompt will not find me."));
            "NO"
        }
        Egg::Who => {
            a.note(format_args!("I am `ask`: I answer plain questions about this machine from what its services already know, and I change nothing."));
            a.note(format_args!("Godspeed is a capability microkernel small enough to explain on a whiteboard in thirty minutes (CLAUDE.md 26.11)."));
            a.note(format_args!("ask \"what to say?\" lists what I can answer."));
            "ASK, ON GODSPEED"
        }
    }
}

/// Where the glossary is published, for "read more". The machine may be offline, so the definition comes
/// first and this is an extra.
pub const GLOSSARY_URL: &str = "https://godspeedhq.github.io/godspeed-os/glossary.html";

/// A term reduced for comparison: ASCII letters and digits, lower case (`AMD-Vi` is `amdvi`).
fn squash(t: &[u8], out: &mut [u8; 32]) -> usize {
    let mut n = 0usize;
    for &c in t {
        if c.is_ascii_alphanumeric() && n < out.len() { out[n] = c.to_ascii_lowercase(); n += 1; }
    }
    n
}

/// Edit distance, bounded: anything over 2 is reported as 3. Enough to read `immou` as `iommu`.
fn near(a: &[u8], b: &[u8]) -> usize {
    if a.len().abs_diff(b.len()) > 2 { return 3; }
    let mut prev = [0usize; 33];
    let mut cur = [0usize; 33];
    for j in 0..=b.len() { prev[j] = j; }
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let sub = prev[j - 1] + if a[i - 1] == b[j - 1] { 0 } else { 1 };
            cur[j] = sub.min(prev[j] + 1).min(cur[j - 1] + 1);
        }
        prev = cur;
    }
    prev[b.len()].min(3)
}

/// "What is X?" from the glossary - `GLOSSARY.md`, read at build time, the only source. An exact match
/// is answered; a near one (one or two letters off, from four letters up) is answered and SAID to be a
/// reading; two equally near is a question back; nothing near is "not in the glossary", never a guess.
pub fn define(term: &[u8], glossary: &[(&str, &str, &str)], a: &mut Answer) {
    let mut t = [0u8; 32];
    let tn = squash(term, &mut t);
    let shown = core::str::from_utf8(term).unwrap_or("?");
    let mut exact = None;
    let mut best: Option<(usize, usize)> = None; // (index, distance)
    let mut tie = false;
    for (i, (abbr, _, _)) in glossary.iter().enumerate() {
        let mut g = [0u8; 32];
        let gn = squash(abbr.as_bytes(), &mut g);
        if gn == 0 { continue; }
        if t[..tn] == g[..gn] { exact = Some(i); break; }
        if tn >= 4 {
            let d = near(&t[..tn], &g[..gn]);
            if d <= 2 {
                match best {
                    Some((_, bd)) if d > bd => {}
                    Some((_, bd)) if d == bd => tie = true,
                    _ => { best = Some((i, d)); tie = false; }
                }
            }
        }
    }
    let pick = match (exact, best) {
        (Some(i), _) => Some(i),
        (None, Some((i, _))) if !tie => {
            a.note(format_args!("I read `{}` as {} - one or two letters off", shown, glossary[i].0));
            Some(i)
        }
        _ => None,
    };
    match pick {
        Some(i) => {
            let (abbr, exp, meaning) = glossary[i];
            let at = a.n;
            a.verdict(format_args!("{}: {}", abbr, exp));
            // The verdict leads even when a "read as" note was said first.
            if at > 0 { a.lines[..=at].rotate_right(1); }
            // The meaning, wrapped at word boundaries to the line size.
            let mb = meaning.as_bytes();
            let mut start = 0usize;
            while start < mb.len() {
                let mut end = (start + TEXT_MAX - 8).min(mb.len());
                if end < mb.len() {
                    if let Some(sp) = mb[start..end].iter().rposition(|&c| c == b' ') { end = start + sp; }
                }
                a.fact("glossary", format_args!("{}", core::str::from_utf8(&mb[start..end]).unwrap_or("")));
                start = end;
                while start < mb.len() && mb[start] == b' ' { start += 1; }
            }
            a.note(format_args!("read more: {}", GLOSSARY_URL));
        }
        None if tie => {
            a.status = Status::Ambiguous;
            a.note(format_args!("`{}` is close to more than one term in the glossary - spell it out?", shown));
        }
        None => {
            a.verdict(format_args!("NOT IN THE GLOSSARY"));
            a.note(format_args!("`{}` is not in GLOSSARY.md, and I do not guess a definition", shown));
            a.note(format_args!("the glossary: {}", GLOSSARY_URL));
        }
    }
    a.finish();
}

/// What `ask` can answer, for `ask help` and for a question it did not understand.
pub fn can_answer(a: &mut Answer) {
    a.note(format_args!("I can answer questions about: system health, what failed, the WiFi restarting, the filesystem"));
    a.note(format_args!("and why a service restarted, failed, crashed or stopped - name it, as in: why did fs restart?"));
    a.note(format_args!("ask \"what to say?\" lists them with suggestions from this machine; ask \"what is MISCIS?\" defines a term"));
}

/// Answer a reading that is not one question kind.
pub fn refuse(r: Reading, q_first_word: &str, known: &[&str], a: &mut Answer) {
    match r {
        Reading::Two(x, y) => {
            a.status = Status::Ambiguous;
            a.note(format_args!("that could be a question about {} or about {} - which?", x.word(known), y.word(known)));
        }
        Reading::Action => {
            a.status = Status::Action;
            a.note(format_args!("I answer questions about the system; I cannot {} anything.", q_first_word));
        }
        Reading::Empty => {
            a.status = Status::Empty;
            can_answer(a);
        }
        Reading::NotUnderstood | Reading::One(..) | Reading::Define { .. } => {
            a.status = Status::NotUnderstood;
            a.note(format_args!("I don't understand that question yet."));
            can_answer(a);
        }
    }
    a.finish();
}

// ---------------------------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    /// The names a QEMU machine knows, as `main.rs` would hand them over.
    const KNOWN: &[&str] = &["supervisor", "events", "fs", "block-driver", "nic-driver", "xhci", "shell", "wifi-usb"];
    fn r(q: &str) -> Reading { understand(q.as_bytes(), &[]) }
    fn rk(q: &str) -> Reading { understand(q.as_bytes(), KNOWN) }

    // ---- Language: every question version 1 understood stays understood.
    #[test]
    fn the_four_questions() {
        assert_eq!(r("is my system healthy?"), Reading::One(Kind::Health, Event::Unnamed));
        assert_eq!(r("what failed?"), Reading::One(Kind::Failed, Event::Unnamed));
        assert_eq!(r("why did wifi restart?"), Reading::One(Kind::Wifi, Event::Restart));
        assert_eq!(r("is the filesystem healthy?"), Reading::One(Kind::Fs, Event::Unnamed));
    }

    #[test]
    fn case_punctuation_and_spelling() {
        assert_eq!(r("IS MY SYSTEM HEALTHY"), Reading::One(Kind::Health, Event::Unnamed));
        assert_eq!(r("  is   the   machine ok ?? "), Reading::One(Kind::Health, Event::Unnamed));
        assert_eq!(r("why did the wi-fi restart"), Reading::One(Kind::Wifi, Event::Restart));
        assert_eq!(r("why did the wi fi restart"), Reading::One(Kind::Wifi, Event::Restart));
        assert_eq!(r("why did the wireless drop?"), Reading::One(Kind::Wifi, Event::Disconnect));
        assert_eq!(r("is the file system ok"), Reading::One(Kind::Fs, Event::Unnamed));
        assert_eq!(r("is the disk healthy"), Reading::One(Kind::Fs, Event::Unnamed));
        assert_eq!(r("has anything crashed?"), Reading::One(Kind::Failed, Event::Unnamed));
    }

    #[test]
    fn events_are_never_merged() {
        assert_eq!(r("why did wifi disconnect"), Reading::One(Kind::Wifi, Event::Disconnect));
        assert_eq!(r("why did wifi crash"), Reading::One(Kind::Wifi, Event::Crash));
        assert_eq!(r("why did wifi reset"), Reading::One(Kind::Wifi, Event::Reset));
        assert_eq!(r("is the wifi ok"), Reading::One(Kind::Wifi, Event::Unnamed));
    }

    #[test]
    fn a_subsystem_word_decides() {
        // Not the system-wide health question, and not "what failed".
        assert_eq!(r("did the disk fail?"), Reading::One(Kind::Fs, Event::Unnamed));
        assert_eq!(r("why did the wifi fail"), Reading::One(Kind::Wifi, Event::Fail));
    }

    #[test]
    fn ambiguous_and_not_understood() {
        assert_eq!(r("is the wifi or the disk broken"), Reading::Two(Kind::Wifi, Kind::Fs));
        assert_eq!(r("is anything wrong or has something crashed"), Reading::Two(Kind::Health, Kind::Failed));
        assert_eq!(r("why is my computer sad?"), Reading::NotUnderstood);
        assert_eq!(r("hello"), Reading::NotUnderstood);
        assert_eq!(r(""), Reading::Empty);
        assert_eq!(r("  ?! "), Reading::Empty);
    }

    #[test]
    fn a_request_to_act_is_not_a_question() {
        assert_eq!(r("restart the wifi driver"), Reading::Action);
        assert_eq!(r("format the disk"), Reading::Action);
        assert_eq!(r("fix it"), Reading::Action);
        // ...but a question about a restart is a question.
        assert_eq!(r("why did the wifi restart"), Reading::One(Kind::Wifi, Event::Restart));
    }

    #[test]
    fn a_long_question_is_read_to_its_bound_not_past_it() {
        let mut q = [b'a'; 2000];
        q[1990..].copy_from_slice(b" healthy??");
        // The word past the bound is not seen, so this is not understood rather than misread.
        assert_eq!(understand(&q, &[]), Reading::NotUnderstood);
    }

    // ---- Evidence fixtures (design section 8): what each answer MUST and MUST NOT say.
    const ALL: &[&str] = &["supervisor", "events", "fs", "shell"];

    fn tasks_ok() -> [Task<'static>; 4] {
        [Task { name: "supervisor", restarts: 0, up_secs: 100 }, Task { name: "events", restarts: 0, up_secs: 100 },
         Task { name: "fs", restarts: 0, up_secs: 100 }, Task { name: "shell", restarts: 0, up_secs: 100 }]
    }
    fn mounted() -> Got<Drive<'static>> {
        Got::Yes(Drive { capacity: 32768, mounted: true, total_blocks: 4096, used_blocks: 1024, label: "data" })
    }
    fn ev_none() -> Got<Events<'static>> { Got::Yes(Events { since: 5, recorded: 0, list: &[] }) }
    fn ask(kind: Kind, event: Event, f: &Facts) -> Answer {
        let mut a = Answer::new();
        assess(kind, event, KNOWN, f, &mut a);
        a
    }
    fn verdict(a: &Answer) -> &[u8] { a.lines()[0].text() }

    #[test]
    fn healthy_only_when_every_required_check_ran() {
        let t = tasks_ok();
        let f = Facts { now: 100, tick: 100, tasks: &t, expected: Got::Yes(ALL), events: ev_none(), drive: mounted(), radios: &[], chain: None };
        let a = ask(Kind::Health, Event::Unnamed, &f);
        assert_eq!(verdict(&a), b"HEALTHY");
        assert_eq!(a.status, Status::Answered);
        assert!(a.has(Label::Fact, "taken at uptime 100s"));
    }

    #[test]
    fn a_silent_source_is_a_hole_not_a_pass() {
        let t = tasks_ok();
        let f = Facts { now: 100, tick: 100, tasks: &t, expected: Got::Silent, events: ev_none(), drive: mounted(), radios: &[], chain: None };
        let a = ask(Kind::Health, Event::Unnamed, &f);
        assert_eq!(verdict(&a), b"UNKNOWN");
        assert_eq!(a.status, Status::SourceSilent);
        assert!(a.has(Label::Unknown, "did not answer"));
        assert!(!a.any_text("HEALTHY") || verdict(&a) != b"HEALTHY");
    }

    #[test]
    fn a_required_service_down_is_unhealthy() {
        let t = [Task { name: "supervisor", restarts: 0, up_secs: 100 }, Task { name: "shell", restarts: 0, up_secs: 9 }];
        let f = Facts { now: 100, tick: 100, tasks: &t, expected: Got::Yes(ALL), events: ev_none(), drive: Got::Absent, radios: &[], chain: None };
        let a = ask(Kind::Health, Event::Unnamed, &f);
        assert_eq!(verdict(&a), b"UNHEALTHY");
        assert!(a.has(Label::Fact, "`fs` is a service the supervisor runs, and it is not running now"));
        assert!(a.has(Label::Inference, "UNHEALTHY"));
    }

    #[test]
    fn a_restart_is_degraded_and_never_called_a_crash() {
        let mut t = tasks_ok();
        t[2].restarts = 2;
        let f = Facts { now: 100, tick: 100, tasks: &t, expected: Got::Yes(ALL), events: ev_none(), drive: mounted(), radios: &[], chain: None };
        let a = ask(Kind::Health, Event::Unnamed, &f);
        assert_eq!(verdict(&a), b"DEGRADED");
        assert!(a.has(Label::Fact, "`fs` has restarted 2 time(s)"));
        assert!(a.has(Label::Unknown, "deliberate `kill`"));
        assert!(!a.any_text("crashed"));
    }

    #[test]
    fn no_disk_is_said_and_is_not_a_fault() {
        let t = tasks_ok();
        let f = Facts { now: 1, tick: 1, tasks: &t, expected: Got::Yes(ALL), events: ev_none(),
                        drive: Got::Yes(Drive { capacity: 0, mounted: false, total_blocks: 0, used_blocks: 0, label: "" }), radios: &[], chain: None };
        let a = ask(Kind::Health, Event::Unnamed, &f);
        assert_eq!(verdict(&a), b"HEALTHY");
        assert!(a.has(Label::Fact, "no storage device is attached"));
        let a = ask(Kind::Fs, Event::Unnamed, &f);
        assert_eq!(verdict(&a), b"NO DISK");
    }

    #[test]
    fn what_failed_keeps_restart_failed_and_down_apart() {
        let ev = [Ev { at: 40, code: EV_RESTARTED, name: "xhci" }, Ev { at: 50, code: EV_RESTART_FAILED, name: "nic-driver" },
                  Ev { at: 60, code: EV_ATTACHED, name: "wifi-usb" }];
        let t = tasks_ok();
        let f = Facts { now: 100, tick: 100, tasks: &t, expected: Got::Yes(&["supervisor", "fs", "nic-driver"]),
                        events: Got::Yes(Events { since: 5, recorded: 40, list: &ev }), drive: mounted(), radios: &[], chain: None };
        let a = ask(Kind::Failed, Event::Unnamed, &f);
        assert_eq!(verdict(&a), b"SOMETHING IS DOWN NOW");
        assert!(a.has(Label::Fact, "at uptime 40s `xhci` died and was restarted"));
        assert!(a.has(Label::Fact, "`nic-driver` died, and its restart FAILED"));
        assert!(a.has(Label::Fact, "`nic-driver` should be running and is not"));
        // An arrival is not a failure.
        assert!(!a.any_text("wifi-usb"));
        // The bounded record says it is bounded.
        assert!(a.has(Label::Fact, "37 earlier event(s) were overwritten"));
        assert!(a.has(Label::Unknown, "a restart is not proof of a crash"));
    }

    #[test]
    fn nothing_recorded_is_not_nothing_wrong() {
        let t = tasks_ok();
        let f = Facts { now: 100, tick: 100, tasks: &t, expected: Got::Yes(ALL), events: ev_none(), drive: mounted(), radios: &[], chain: None };
        let a = ask(Kind::Failed, Event::Unnamed, &f);
        assert_eq!(verdict(&a), b"NOTHING RECORDED");
        assert!(a.has(Label::Inference, "not proof nothing went wrong"));
    }

    #[test]
    fn wifi_with_no_radio_says_so() {
        let t = tasks_ok();
        let f = Facts { now: 100, tick: 100, tasks: &t, expected: Got::Yes(ALL), events: ev_none(), drive: mounted(), radios: &[], chain: None };
        let a = ask(Kind::Wifi, Event::Restart, &f);
        assert_eq!(verdict(&a), b"NO RADIO");
        assert!(a.has(Label::Fact, "no radio driver"));
    }

    #[test]
    fn a_firmware_fault_before_a_restart_is_what_came_before_not_why() {
        let t = [Task { name: "wifi-driver", restarts: 1, up_secs: 30 }];
        let radios = [RadioFact { driver: "wifi-driver", status: Got::Yes(Radio::Down(DOWN_TRAPPED)) }];
        let ev = [Ev { at: 70, code: EV_RESTARTED, name: "wifi-driver" }];
        let f = Facts { now: 100, tick: 100, tasks: &t, expected: Got::Yes(&["wifi-driver"]),
                        events: Got::Yes(Events { since: 5, recorded: 1, list: &ev }), drive: Got::Absent, radios: &radios, chain: None };
        let a = ask(Kind::Wifi, Event::Restart, &f);
        assert_eq!(verdict(&a), b"THE RADIO DRIVER RESTARTED");
        assert!(a.has(Label::Fact, "trapped at start"));
        assert!(a.has(Label::Fact, "at uptime 70s `wifi-driver` died and was restarted"));
        assert!(a.has(Label::Note, "nothing records that it caused the restart"));
        assert!(a.has(Label::Unknown, "why `wifi-driver` died is not recorded"));
        // NEVER: "the firmware caused it".
        assert!(!a.any_text("caused the restart because") && !a.any_text("because the firmware"));
    }

    #[test]
    fn a_link_drop_with_no_restart_is_not_a_restart() {
        let t = [Task { name: "wifi-usb", restarts: 0, up_secs: 900 }];
        let radios = [RadioFact { driver: "wifi-usb", status: Got::Yes(Radio::Up { associated: false, ssid: "", joined_secs: 0 }) }];
        let f = Facts { now: 900, tick: 900, tasks: &t, expected: Got::Yes(&["wifi-usb"]), events: ev_none(), drive: Got::Absent, radios: &radios, chain: None };
        let a = ask(Kind::Wifi, Event::Disconnect, &f);
        assert_eq!(verdict(&a), b"NO RESTART RECORDED");
        assert!(a.has(Label::Fact, "has not restarted since boot"));
        assert!(a.has(Label::Unknown, "when or why the link dropped"));
        assert!(a.has(Label::Note, "drop the link"));
    }

    #[test]
    fn a_mounted_filesystem_is_not_called_healthy() {
        let t = tasks_ok();
        let f = Facts { now: 100, tick: 100, tasks: &t, expected: Got::Yes(ALL), events: ev_none(), drive: mounted(), radios: &[], chain: None };
        let a = ask(Kind::Fs, Event::Unnamed, &f);
        assert_eq!(verdict(&a), b"MOUNTED - NOT VERIFIED");
        assert!(a.has(Label::Unknown, "journal was replayed"));
        assert!(a.has(Label::Note, "`drives scrub`"));
        assert!(!a.any_text("HEALTHY"));
        let f2 = Facts { drive: Got::Silent, ..f };
        let a = ask(Kind::Fs, Event::Unnamed, &f2);
        assert_eq!(a.status, Status::SourceSilent);
    }

    #[test]
    fn refusals() {
        let mut a = Answer::new();
        refuse(r("restart the wifi driver"), "restart", KNOWN, &mut a);
        assert_eq!(a.status, Status::Action);
        assert!(a.any_text("I cannot restart anything"));
        let mut a = Answer::new();
        refuse(r("why is my computer sad?"), "why", KNOWN, &mut a);
        assert_eq!(a.status, Status::NotUnderstood);
        assert!(a.any_text("system health, what failed, the WiFi restarting, the filesystem"));
        let mut a = Answer::new();
        refuse(r("is the wifi or the disk broken"), "is", KNOWN, &mut a);
        assert_eq!(a.status, Status::Ambiguous);
        assert!(a.any_text("the WiFi or about the filesystem"));
    }

    #[test]
    fn an_answer_that_overflows_says_it_is_partial() {
        let mut t = [Task { name: "x", restarts: 1, up_secs: 1 }; 80];
        for (i, x) in t.iter_mut().enumerate() { x.restarts = i as u64 + 1; }
        let f = Facts { now: 1, tick: 1, tasks: &t, expected: Got::Yes(&[]), events: ev_none(), drive: mounted(), radios: &[], chain: None };
        let a = ask(Kind::Health, Event::Unnamed, &f);
        assert!(a.lines().len() <= LINES_MAX);
        assert!(a.has(Label::Note, "PARTIAL"));
    }

    // ---- The general question: a service named, with an event. Names come from the machine.
    #[test]
    fn why_did_a_named_service_restart_or_fail() {
        assert_eq!(rk("why did fs service restart"), Reading::One(Kind::Service(2), Event::Restart));
        assert_eq!(rk("why did fs service fail"), Reading::One(Kind::Service(2), Event::Fail));
        assert_eq!(rk("did the xhci driver crash?"), Reading::One(Kind::Service(5), Event::Crash));
        // A hyphenated name is matched as words, and the longest name wins over a shorter one inside it.
        assert_eq!(rk("why did nic-driver restart"), Reading::One(Kind::Service(4), Event::Restart));
        assert_eq!(rk("why did the nic driver stop"), Reading::One(Kind::Service(4), Event::Stop));
        // Two services named at once is a question this must not settle.
        assert_eq!(rk("why did fs and xhci restart"), Reading::Two(Kind::Service(2), Kind::Service(5)));
        // Without an event, a subsystem question stays the subsystem question.
        assert_eq!(rk("is the fs healthy"), Reading::One(Kind::Fs, Event::Unnamed));
        // The WiFi stays an alias: "the wifi" names no service.
        assert_eq!(rk("why did the wifi restart"), Reading::One(Kind::Wifi, Event::Restart));
        // A request to act on a named service is still not a question.
        assert_eq!(rk("restart fs"), Reading::Action);
        // An unknown name is not invented into a service.
        assert_eq!(rk("why did frobnicator restart"), Reading::One(Kind::Failed, Event::Unnamed));
    }

    #[test]
    fn a_restart_is_not_called_a_failure() {
        let mut t = tasks_ok();
        t[2].restarts = 3;
        let ev = [Ev { at: 70, code: EV_RESTARTED, name: "fs" }];
        let f = Facts { now: 100, tick: 100, tasks: &t, expected: Got::Yes(ALL),
                        events: Got::Yes(Events { since: 5, recorded: 1, list: &ev }), drive: mounted(), radios: &[], chain: None };
        let a = ask(Kind::Service(2), Event::Fail, &f);
        assert_eq!(verdict(&a), b"`fs` HAS RESTARTED 3 TIME(S) - WHY IS NOT RECORDED");
        assert!(a.has(Label::Fact, "`fs` has restarted 3 time(s) since boot"));
        assert!(a.has(Label::Fact, "at uptime 70s `fs` died and was restarted"));
        assert!(a.has(Label::Unknown, "whether it FAILED cannot be said"));
        assert!(a.has(Label::Note, "did fail; a restart, a failure"));
        assert!(!a.any_text("it failed because"));
    }

    #[test]
    fn a_named_service_that_is_down_or_never_restarted() {
        let t = [Task { name: "supervisor", restarts: 0, up_secs: 100 }];
        let f = Facts { now: 100, tick: 100, tasks: &t, expected: Got::Yes(ALL), events: ev_none(), drive: Got::Absent, radios: &[], chain: None };
        let a = ask(Kind::Service(2), Event::Restart, &f);
        assert_eq!(verdict(&a), b"`fs` IS DOWN NOW");
        assert!(a.has(Label::Fact, "should be up"));
        let t = tasks_ok();
        let f = Facts { now: 100, tick: 100, tasks: &t, expected: Got::Yes(ALL), events: ev_none(), drive: mounted(), radios: &[], chain: None };
        let a = ask(Kind::Service(0), Event::Restart, &f);
        assert_eq!(verdict(&a), b"`supervisor` HAS NOT RESTARTED SINCE BOOT");
        assert!(!a.has(Label::Unknown, "why"));
    }

    #[test]
    fn a_slow_fs_is_a_hole_not_unhealthy() {
        let t = tasks_ok();
        let f = Facts { now: 100, tick: 100, tasks: &t, expected: Got::Yes(ALL), events: ev_none(), drive: Got::Silent, radios: &[], chain: None };
        let a = ask(Kind::Health, Event::Unnamed, &f);
        assert_eq!(verdict(&a), b"UNKNOWN");
        assert_eq!(a.status, Status::SourceSilent);
        assert!(!a.any_text("UNHEALTHY"));
    }

    #[test]
    fn no_radio_answering_is_not_called_a_fault() {
        let t = tasks_ok();
        let radios = [RadioFact { driver: "wifi-driver", status: Got::Yes(Radio::Down(DOWN_NO_RADIO)) }];
        let f = Facts { now: 100, tick: 100, tasks: &t, expected: Got::Yes(ALL), events: ev_none(), drive: mounted(), radios: &radios, chain: None };
        let a = ask(Kind::Health, Event::Unnamed, &f);
        assert_eq!(verdict(&a), b"HEALTHY");
        assert!(a.has(Label::Unknown, "whether this machine has a radio fitted"));
        let radios = [RadioFact { driver: "wifi-driver", status: Got::Yes(Radio::Down(DOWN_TRAPPED)) }];
        let f = Facts { radios: &radios, ..f };
        assert_eq!(verdict(&ask(Kind::Health, Event::Unnamed, &f)), b"DEGRADED");
    }

    #[test]
    fn what_to_say() {
        for q in ["what to say?", "what can I ask?", "what questions can I ask", "help", "help me"] {
            assert_eq!(rk(q), Reading::One(Kind::Guide, Event::Unnamed), "{}", q);
        }
        // Still the questions they were.
        assert_eq!(rk("what failed?"), Reading::One(Kind::Failed, Event::Unnamed));
        assert_eq!(rk("why did fs restart"), Reading::One(Kind::Service(2), Event::Restart));
    }

    #[test]
    fn suggestions_come_from_this_machine() {
        let mut t = tasks_ok();
        t[2].restarts = 4; // fs
        t[3].restarts = 1; // shell
        let f = Facts { now: 100, tick: 100, tasks: &t, expected: Got::Yes(&["supervisor", "events", "fs", "shell", "block-driver"]),
                        events: ev_none(), drive: mounted(), radios: &[], chain: None };
        let a = ask(Kind::Guide, Event::Unnamed, &f);
        assert_eq!(verdict(&a), b"YOU CAN ASK");
        assert!(a.has(Label::Note, "try: ask \"why did fs restart?\"  - it has restarted 4 time(s)"));
        assert!(a.has(Label::Note, "why did shell restart?"));
        assert!(a.has(Label::Note, "why did block-driver stop?"));
        // No radio here, so no WiFi suggestion.
        assert!(!a.any_text("wifi restart?\"  - a radio"));
        // The most-restarted comes first.
        let pos = |n: &str| a.lines().iter().position(|l| contains(l.text(), n.as_bytes()));
        assert!(matches!((pos("why did fs restart"), pos("why did shell restart")), (Some(x), Some(y)) if x < y));
    }

    #[test]
    fn easter_eggs_are_found_and_say_what_they_are() {
        assert_eq!(rk("tell me a joke"), Reading::One(Kind::Egg(Egg::Joke), Event::Unnamed));
        assert_eq!(rk("what is the meaning of life?"), Reading::One(Kind::Egg(Egg::Meaning), Event::Unnamed));
        assert_eq!(rk("sudo make me a sandwich"), Reading::One(Kind::Egg(Egg::Sudo), Event::Unnamed));
        assert_eq!(rk("rm -rf /"), Reading::One(Kind::Egg(Egg::RmRf), Event::Unnamed));
        assert_eq!(rk("are you alive?"), Reading::One(Kind::Egg(Egg::Alive), Event::Unnamed));
        assert_eq!(rk("who are you?"), Reading::One(Kind::Egg(Egg::Who), Event::Unnamed));
        assert_eq!(rk("what is godspeed?"), Reading::One(Kind::Egg(Egg::Who), Event::Unnamed));
        // The real questions are untouched.
        assert_eq!(rk("is my system healthy?"), Reading::One(Kind::Health, Event::Unnamed));
        assert_eq!(rk("restart fs"), Reading::Action);
        // A joke's verdict says it is one, and two seconds give two jokes.
        let t = tasks_ok();
        let f = Facts { now: 100, tick: 100, tasks: &t, expected: Got::Yes(ALL), events: ev_none(), drive: mounted(), radios: &[], chain: None };
        let a = ask(Kind::Egg(Egg::Joke), Event::Unnamed, &f);
        assert_eq!(verdict(&a), b"A JOKE");
        assert!(a.has(Label::Note, "not an observation"));
        let f2 = Facts { now: 100, tick: 101, ..f };
        let b = ask(Kind::Egg(Egg::Joke), Event::Unnamed, &f2);
        assert!(a.lines()[1].text() != b.lines()[1].text());
        let m = ask(Kind::Egg(Egg::Meaning), Event::Unnamed, &f);
        assert!(m.has(Label::Unknown, "why is not recorded"));
    }

    #[test]
    fn storage_chain_names_the_first_link_that_stops() {
        let t = tasks_ok();
        let none = Got::Yes(Drive { capacity: 0, mounted: false, total_blocks: 0, used_blocks: 0, label: "" });
        // QEMU on the Pi 4: block-driver answers 0, and xhci - the host it is behind - is silent.
        let f = Facts { now: 9, tick: 9, tasks: &t, expected: Got::Yes(ALL), events: ev_none(), drive: none, radios: &[],
                        chain: Some(Chain { block: Got::Yes(0), host: Some(("xhci", Got::Silent)), host_read: true }) };
        let a = ask(Kind::Fs, Event::Unnamed, &f);
        assert_eq!(verdict(&a), b"STORAGE STOPS AT xhci");
        assert!(a.has(Label::Fact, "`block-driver` reaches its disk through `xhci`"));
        assert!(a.has(Label::Inference, "`xhci`"));
        // Both silent: the deeper one is named, and the waiting one is not blamed.
        let fb = Facts { chain: Some(Chain { block: Got::Silent, host: Some(("xhci", Got::Silent)), host_read: true }), ..f };
        let ab = ask(Kind::Fs, Event::Unnamed, &fb);
        assert_eq!(verdict(&ab), b"STORAGE STOPS AT xhci");
        assert!(ab.has(Label::Unknown, "may only be waiting on `xhci`"));
        // Every link answers and none has a disk: nothing attached, nothing broken.
        let f2 = Facts { chain: Some(Chain { block: Got::Yes(0), host: Some(("dwc2", Got::Yes(0))), host_read: true }), ..f };
        let b = ask(Kind::Fs, Event::Unnamed, &f2);
        assert_eq!(verdict(&b), b"NO DISK ATTACHED");
        assert!(b.has(Label::Inference, "rather than anything being broken"));
        // A PC: block-driver drives AHCI itself, and is down.
        let f3 = Facts { chain: Some(Chain { block: Got::Absent, host: None, host_read: true }), ..f2 };
        let c = ask(Kind::Fs, Event::Unnamed, &f3);
        assert_eq!(verdict(&c), b"STORAGE STOPS AT block-driver");
        assert!(c.has(Label::Fact, "drives its disk itself"));
        // A mounted filesystem never walks the chain.
        let f4 = Facts { drive: mounted(), ..f3 };
        assert_eq!(verdict(&ask(Kind::Fs, Event::Unnamed, &f4)), b"MOUNTED - NOT VERIFIED");
    }

    const GLOSS: &[(&str, &str, &str)] = &[
        ("IOMMU", "Input/Output Memory Management Unit", "Translates every device DMA through a per-device page table."),
        ("MISCIS", "Memory isolation, IPC, Scheduling, Capabilities, Interrupts, SMP routing", "The kernel's six responsibilities, said like \"misses\"."),
        ("MMIO", "Memory-Mapped I/O", "Device registers accessed as memory."),
        ("MSI", "Message-Signaled Interrupt", "PCI interrupt delivery via a memory write."),
        ("AMD-Vi", "AMD Virtualization (AMD's IOMMU)", "The IOMMU behind H1."),
    ];
    fn def(q: &str) -> Answer {
        let mut a = Answer::new();
        match rk(q) {
            Reading::Define { at, len } => define(&q.as_bytes()[at..at + len], GLOSS, &mut a),
            // Not a definition: an answer whose verdict no assertion expects, so the test fails by saying so.
            other => a.verdict(format_args!("MISREAD {} AS {:?}", q, other)),
        }
        a
    }

    #[test]
    fn what_is_a_term() {
        let a = def("what is miscis?");
        assert_eq!(verdict(&a), b"MISCIS: Memory isolation, IPC, Scheduling, Capabilities, Interrupts, SMP routing");
        assert!(a.has(Label::Fact, "six responsibilities"));
        assert!(a.has(Label::Note, "read more: https://godspeedhq.github.io/godspeed-os/glossary.html"));
        // Hyphens and case do not matter.
        assert!(verdict(&def("what does amd-vi mean?")).starts_with(b"AMD-Vi:"));
        assert!(verdict(&def("define MMIO")).starts_with(b"MMIO:"));
    }

    #[test]
    fn a_typo_is_read_and_said_to_be() {
        let a = def("what is immou?");
        assert_eq!(&verdict(&a)[..6], b"IOMMU:");
        assert!(a.has(Label::Note, "I read `immou` as IOMMU"));
        // A short word is never stretched to a term.
        let b = def("what is msx?");
        assert_eq!(verdict(&b), b"NOT IN THE GLOSSARY");
    }

    #[test]
    fn a_term_not_in_the_glossary_is_never_guessed() {
        let a = def("what is quantumflux?");
        assert_eq!(verdict(&a), b"NOT IN THE GLOSSARY");
        assert!(a.has(Label::Note, "I do not guess a definition"));
    }

    #[test]
    fn definitions_do_not_take_the_real_questions() {
        assert_eq!(rk("what is wrong?"), Reading::One(Kind::Health, Event::Unnamed));
        assert_eq!(rk("what is godspeed?"), Reading::One(Kind::Egg(Egg::Who), Event::Unnamed));
        assert_eq!(rk("what is the meaning of life?"), Reading::One(Kind::Egg(Egg::Meaning), Event::Unnamed));
        assert_eq!(rk("what failed?"), Reading::One(Kind::Failed, Event::Unnamed));
    }
}
