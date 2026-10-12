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
// Understanding the question (design section 4). Deterministic, small, no model; pinned by the tests below.
// ---------------------------------------------------------------------------------------------

/// The question kinds (design sections 3 and 4): the four of v0.1, the general "why did <service> <event>"
/// beside them, and those added since - the network, what a service is and what it can do, who holds an
/// authority, what can be asked, and the Easter eggs. A term to define is a `Reading`, not a kind.
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
    /// Is the network up? Why can't I reach the internet? Answered from `net-stack`'s status - the
    /// address, the lease, the gateway - and the radio where the link is wireless.
    Network,
    /// Why did (did, has) this service <event>? The SUBJECT is not vocabulary: it is an index into the
    /// names the machine itself knows (`understand`'s `known`), so a new service needs no new word.
    Service(usize),
    /// What is this service? From its own crate's description (`Cargo.toml`, read at build time) and the
    /// task table. A service is named from `known`, as for `Service`.
    About(usize),
    /// What can this service do? The kernel authorities and the services its capabilities reach, read
    /// from the kernel (INTROSPECT) - what it CAN do, never what it has done.
    Reach(usize),
    /// Who can do this? The tasks that hold one kernel authority - its resource id (`AUTHORITIES`), or 0
    /// for a word that names none.
    WhoCan(u64),
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
            Kind::Network => "the network",
            Kind::Service(i) | Kind::About(i) | Kind::Reach(i) => known.get(i).copied().unwrap_or("a service"),
            Kind::WhoCan(_) => "who holds an authority",
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
const WIFI_WORDS: &[&str] = &["wifi", "wireless", "wlan", "radio"];
/// The network as a whole - addresses, the lease, the way out. "network" was a WiFi word, which made
/// "is the network up?" a question about the radio; a cable is a network too.
const NET_WORDS: &[&str] = &["network", "internet", "online", "offline", "connection", "connected", "ip", "dhcp",
                             "lease", "dns", "gateway", "ethernet", "net"];
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

/// The words that name a kernel authority, for "who can ...?" - a verb a person would use, and the
/// authority's own name. Resource ids are the kernel's fixed set (`godspeed_sdk::service_context::AUTHORITIES`,
/// re-exported as `gs::task::AUTHORITIES`, which `main.rs` hands in as `Facts::authorities`); a word missing here is answered "which authority?",
/// never guessed. No clock words: `set_clock` is granted to nothing, and the clock is the `time`
/// service's, so "who can set the clock?" would be answered wrongly by the kernel's table.
const CAN_WORDS: &[(&str, u64)] = &[
    ("log", 1), ("logs", 1),
    ("spawn", 2), ("start", 2), ("launch", 2),
    ("keyboard", 3), ("keys", 3),
    ("type", 4), ("inject", 4), ("push", 4),
    ("introspect", 5), ("inspect", 5), ("observe", 5),
    ("kill", 6), ("restart", 6), ("control", 6),
    ("mint", 7),
    ("reboot", 8), ("reset", 8),
    ("acquire", 9),
    ("gpio", 11), ("pins", 11),
    ("irq", 14), ("interrupt", 14), ("interrupts", 14),
    ("image", 15), ("images", 15),
    ("pci", 16),
    ("power", 17),
    ("cpu", 18),
];

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

    // WHO CAN: "who can reboot?", "which service can kill?", "who holds introspect?" - the tasks holding one
    // kernel authority. Before the definitions, so "who" is never a term.
    if any(w(0), &["who", "which", "whom"]) && has_e(&["can", "may", "holds", "hold", "has", "have", "allowed"]) {
        let rid = (1..n).find_map(|i| CAN_WORDS.iter().find(|(k, _)| k.as_bytes() == w(i)).map(|&(_, r)| r)).unwrap_or(0);
        return Reading::One(Kind::WhoCan(rid), Event::Unnamed);
    }
    // WHAT CAN <service> DO: "what can fs do?", "what can fs reach?", "what does fs hold?", "what
    // capabilities does fs have?" - a named service and a word about authority.
    if w(0) == b"what" && has_e(&["can", "hold", "holds", "capabilities", "caps", "authority", "authorities", "reach", "access"]) {
        let (svc, other) = find_service(&buf, &ws, n, known);
        if let (Some(s), None) = (svc, other) { return Reading::One(Kind::Reach(s), Event::Unnamed); }
    }
    // WHAT IS <service>: "what is fs?", "what does xhci do?", "what's net-stack?" - a service the machine
    // knows is described from its own crate, before the glossary is asked about a word.
    if n <= 5 && (w(0) == b"what" || w(0) == b"whats") && has_e(&["is", "s", "does", "whats"]) && !has_e(&["mean", "stand", "wrong"]) {
        let (svc, other) = find_service(&buf, &ws, n, known);
        if let (Some(s), None) = (svc, other) { return Reading::One(Kind::About(s), Event::Unnamed); }
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

    let (mut wifi, mut fs, mut health, mut failed, mut system, mut net) = (false, false, false, false, false, false);
    let mut event = Event::Unnamed;
    let mut questioned = false;
    for i in 0..n {
        let x = w(i);
        if any(x, QUESTION_WORDS) { questioned = true; }
        if any(x, WIFI_WORDS) { wifi = true; }
        if any(x, NET_WORDS) { net = true; }
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
    // The network, unless the radio is named: "why did the wifi drop?" is about the radio, and "can I
    // reach the internet over wifi?" about the network - the event decides.
    if net && !fs && (!wifi || event == Event::Unnamed) {
        return Reading::One(Kind::Network, Event::Unnamed);
    }
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
    /// `net-stack`'s status, when the network was the question.
    pub net: Got<NetStatus>,
    /// How the service asked about last ended, as the kernel said in its death notice and the supervisor
    /// kept (`supcmd::DEATH`), when a service was the question.
    pub death: Got<Death<'a>>,
    /// The service asked about, in its own crate's words (`Cargo.toml` `description`).
    pub about: Option<&'a str>,
    /// The capabilities of the service asked about, for "what can it do?".
    pub caps: Got<Caps<'a>>,
    /// The tasks that hold the authority asked about, for "who can?", and those whose table could not be read.
    pub holders: Got<Holders<'a>>,
    /// The kernel's fixed authorities: (resource id, name, what it allows) - `godspeed_sdk::service_context::AUTHORITIES`.
    pub authorities: &'a [(u64, &'a str, &'a str)],
}

/// Who holds one authority. A task whose capability table could not be read is listed apart, because
/// "holds nothing" and "could not be asked" are different facts (`backlog/82` A2).
#[derive(Clone, Copy)]
pub struct Holders<'a> {
    /// Running tasks that hold it.
    pub held: &'a [&'a str],
    /// Running tasks whose capability table came back empty - unread, not empty.
    pub unread: &'a [&'a str],
}

/// What one task's capabilities give it, sorted by the kernel's resource ids.
#[derive(Clone, Copy)]
pub struct Caps<'a> {
    /// Kernel authorities it holds (resource ids of `Facts::authorities`).
    pub authorities: &'a [u64],
    /// Running services it can send to.
    pub peers: &'a [&'a str],
    /// Delegated resources it holds (a file `fs` opened for it, for example).
    pub resources: usize,
    /// Endpoints that belong to no running service (a reply mailbox, or a service that died).
    pub other: usize,
}

/// The newest death notice the supervisor read for one service.
#[derive(Clone, Copy)]
pub struct Death<'a> {
    /// When the supervisor began its record, in seconds of uptime.
    pub since: u32,
    /// Notices it has read for this service since then; 0 means none, and the rest is empty.
    pub deaths: u32,
    /// The newest one's uptime second.
    pub secs: u32,
    /// `D_KILLED` or `D_FAULT`, or 0 for a notice that did not say.
    pub how: u8,
    /// `F_*`, for a fault.
    pub kind: u8,
    pub addr: u64,
    /// The task that called `kill`, for a kill.
    pub by: &'a str,
}

/// How a task ended (`death::*` in the SDK, `task::death` in the kernel), restated so this file names
/// nothing outside `core`. `main.rs` asserts at compile time that they match.
pub const D_KILLED: u8 = b'K';
pub const D_FAULT: u8 = b'F';
pub const F_OTHER: u8 = 0;
pub const F_MEMORY: u8 = 1;
pub const F_INSTRUCTION: u8 = 2;
pub const F_ILLEGAL: u8 = 3;
pub const F_PROTECTION: u8 = 4;
pub const F_ALIGNMENT: u8 = 5;

fn fault_words(kind: u8) -> &'static str {
    match kind {
        F_MEMORY => "a memory fault - a load or store its page tables do not allow",
        F_INSTRUCTION => "an instruction-fetch fault - it jumped where it may not execute",
        F_ILLEGAL => "an illegal instruction",
        F_PROTECTION => "a general protection fault",
        F_ALIGNMENT => "an alignment fault",
        _ => "a CPU exception the port does not classify",
    }
}

/// What `net-stack` says it believes about the network (`gs::net::Status`), reduced to what the answer
/// uses.
#[derive(Clone, Copy)]
pub struct NetStatus {
    pub ip: [u8; 4],
    pub gateway: [u8; 4],
    pub dns: [u8; 4],
    /// DHCP granted the address, as opposed to it being unconfigured.
    pub leased: bool,
    /// ARP has resolved the gateway, so traffic can leave this machine.
    pub gateway_resolved: bool,
    /// A ping has succeeded at least once since `net-stack` started - history, not now.
    pub ping_ok: bool,
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
// The assessments (design section 3). Each states its facts, then its inferences by named rule,
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
            Kind::Network => { let _ = v.write_str(network(f, a)); }
            Kind::Service(i) => service(known.get(i).copied().unwrap_or("?"), event, f, a, &mut v),
            Kind::About(i) => about(known.get(i).copied().unwrap_or("?"), f, a, &mut v),
            Kind::Reach(i) => reach(known.get(i).copied().unwrap_or("?"), f, a, &mut v),
            Kind::WhoCan(rid) => who_can(rid, f, a, &mut v),
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

/// Why did (did, has) `name` <event>? From the task table, the supervisor's record and its list of what it
/// keeps running, and - for how it last ended - the kernel's death notice as the supervisor kept it
/// (`supcmd::DEATH`).
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
    // HOW IT ENDED, from the kernel's death notice as the supervisor kept it - the newest one only.
    let mut how: Option<Death> = None;
    match f.death {
        Got::Yes(d) if d.deaths > 0 && d.how == D_KILLED => {
            a.fact("supervisor", format_args!("the kernel's notice of its newest death, at uptime {}s: `{}` was KILLED - `{}` called `kill`", d.secs, name, d.by));
            a.infer("killed-not-failed", format_args!("a kill is asked for, so that death was not a failure of `{}` itself", name));
            a.unknown(format_args!("why `{}` killed it is `{}`'s to say; the kernel records who, not why", d.by, d.by));
            how = Some(d);
        }
        Got::Yes(d) if d.deaths > 0 && d.how == D_FAULT => {
            a.fact("supervisor", format_args!("the kernel's notice of its newest death, at uptime {}s: `{}` ended on {}, at address {:#x}", d.secs, name, fault_words(d.kind), d.addr));
            if d.kind == F_MEMORY && d.addr < 0x1000 {
                a.infer("null-page", format_args!("an access in the first page is a null pointer - and a service that PANICS ends exactly this way: its panic handler reads address 0 so the kernel kills it"));
                a.note(format_args!("a panic prints `PANIC in service:` and its reason on the serial console just before the fault"));
            } else {
                a.infer("crashed", format_args!("a CPU fault in the service's own code is a crash: nothing asked for it"));
            }
            how = Some(d);
        }
        Got::Yes(d) if d.deaths > 0 => {
            a.fact("supervisor", format_args!("the supervisor read a death notice for `{}` at uptime {}s that did not say how it ended", name, d.secs));
        }
        Got::Yes(d) => {
            a.fact("supervisor", format_args!("no death notice for `{}` since uptime {}s, when this supervisor began its record", name, d.since));
        }
        Got::Silent => hole(a, "the supervisor"),
        Got::Absent => {}
    }
    if let (Got::Yes(d), true) = (f.death, how.is_some()) {
        if (d.deaths as u64) < restarts {
            a.note(format_args!("that is the newest of {} restart(s); the supervisor keeps how each service last ended, and its record began at uptime {}s", restarts, d.since));
        } else if d.deaths > 1 {
            a.note(format_args!("it has died {} time(s) since uptime {}s; only the newest is kept", d.deaths, d.since));
        }
    }
    if how.is_none() && (restarts > 0 || recorded > 0 || task.is_none()) {
        a.unknown(format_args!("why `{}` died is not recorded anywhere this can read", name));
        if matches!(event, Event::Fail | Event::Crash) {
            a.unknown(format_args!("so whether it FAILED cannot be said: a deliberate `kill` restarts a service too, and the record does not say which"));
        }
    }
    if let Some(d) = how {
        let crashed = d.how == D_FAULT;
        let _ = match (crashed, task.is_some()) {
            (false, true) => write!(v, "`{}` WAS KILLED BY `{}`, AND IS RUNNING AGAIN", name, d.by),
            (false, false) => write!(v, "`{}` WAS KILLED BY `{}`", name, d.by),
            (true, true) => write!(v, "`{}` CRASHED, AND IS RUNNING AGAIN", name),
            (true, false) => write!(v, "`{}` CRASHED", name),
        };
        return;
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

/// What is <service>? Its own description, and what the task table and the supervisor say about it now.
fn about(name: &str, f: &Facts, a: &mut Answer, v: &mut TextBuf) {
    match f.about {
        Some(d) => {
            a.fact("Cargo.toml", format_args!("that is how `{}` describes itself, in its crate's Cargo.toml", name));
            let _ = write!(v, "`{}`: {}", name, d);
        }
        None => {
            a.unknown(format_args!("`{}` has no description in this build - it is not one of the system's crates", name));
            let _ = write!(v, "`{}`: NO DESCRIPTION", name);
        }
    }
    match f.task(name) {
        Some(t) if t.restarts > 0 => a.fact("task table", format_args!("`{}` is running (up {}s) and has restarted {} time(s) since boot", name, t.up_secs, t.restarts)),
        Some(t) => a.fact("task table", format_args!("`{}` is running (up {}s)", name, t.up_secs)),
        None => a.fact("task table", format_args!("`{}` is not running now", name)),
    }
    if let Got::Yes(names) = f.expected {
        if names.iter().any(|n| *n == name) {
            a.fact("supervisor", format_args!("the supervisor keeps `{}` running, and restarts it if it dies", name));
        } else {
            a.fact("supervisor", format_args!("the supervisor does not keep `{}` running: it is started when something asks for it", name));
        }
    }
    a.note(format_args!("ask \"what can {} do?\" - the authorities and services its capabilities reach", name));
}

/// The kernel authority with this resource id: (name, what it allows).
fn authority<'t>(f: &Facts<'t>, rid: u64) -> Option<(&'t str, &'t str)> {
    f.authorities.iter().find(|(r, _, _)| *r == rid).map(|&(_, n, m)| (n, m))
}

/// What can <service> do? Its capabilities, read from the kernel: what it CAN do, never what it did.
fn reach(name: &str, f: &Facts, a: &mut Answer, v: &mut TextBuf) {
    let c = match f.caps {
        Got::Yes(c) => c,
        Got::Silent => {
            hole(a, "the kernel's capability table");
            let _ = write!(v, "UNKNOWN - `{}`'s capabilities could not be read", name);
            return;
        }
        Got::Absent => {
            a.fact("task table", format_args!("`{}` is not running now, so it holds nothing - a capability lives as long as its task", name));
            let _ = write!(v, "`{}` IS NOT RUNNING", name);
            return;
        }
    };
    for rid in c.authorities {
        match authority(f, *rid) {
            Some((n, m)) => a.fact("kernel", format_args!("`{}` holds `{}` - it can {}", name, n, m)),
            None => a.fact("kernel", format_args!("`{}` holds kernel authority #{}", name, rid)),
        }
    }
    if !c.peers.is_empty() {
        let mut list = [0u8; TEXT_MAX];
        let mut l = 0usize;
        for (k, p) in c.peers.iter().enumerate() {
            let sep: &[u8] = if k == 0 { b"" } else { b", " };
            let pb = p.as_bytes();
            if l + sep.len() + pb.len() + 8 > TEXT_MAX - 40 { break; }
            list[l..l + sep.len()].copy_from_slice(sep); l += sep.len();
            list[l..l + pb.len()].copy_from_slice(pb); l += pb.len();
        }
        a.fact("kernel", format_args!("it can send to {} running service(s): {}", c.peers.len(), core::str::from_utf8(&list[..l]).unwrap_or("")));
    } else {
        a.fact("kernel", format_args!("it can send to no running service"));
    }
    if c.resources > 0 {
        a.fact("kernel", format_args!("it holds {} delegated resource(s), such as a file opened for it", c.resources));
    }
    if c.other > 0 {
        a.fact("kernel", format_args!("and {} endpoint(s) that belong to no running service - a reply mailbox, or a service that has since died", c.other));
    }
    a.note(format_args!("a capability is what `{}` CAN do; what it has done is not recorded", name));
    let _ = write!(v, "`{}` HOLDS {} AUTHORITY(IES) AND CAN SEND TO {} SERVICE(S)", name, c.authorities.len(), c.peers.len());
}

/// Who can <do this>? The running tasks that hold one kernel authority.
fn who_can(rid: u64, f: &Facts, a: &mut Answer, v: &mut TextBuf) {
    let (n, m) = match authority(f, rid) {
        Some(x) => x,
        None => {
            a.status = Status::Ambiguous;
            a.note(format_args!("that names no authority I know; try: reboot, kill, spawn, type, introspect, mint, power, pci"));
            for (_, n, m) in f.authorities.iter().take(6) {
                a.note(format_args!("`{}` - {}", n, m));
            }
            let _ = write!(v, "WHICH AUTHORITY?");
            return;
        }
    };
    let holders = match f.holders {
        Got::Yes(h) => h,
        Got::Silent => {
            hole(a, "the kernel's capability table");
            let _ = write!(v, "UNKNOWN - who holds `{}` could not be read", n);
            return;
        }
        Got::Absent => Holders { held: &[], unread: &[] },
    };
    a.fact("kernel", format_args!("`{}` lets a task {}", n, m));
    for h in holders.held {
        a.fact("kernel", format_args!("`{}` holds `{}`", h, n));
    }
    // A table that could not be read is a hole, never "holds nothing" (a hole is never a pass).
    for u in holders.unread {
        a.unknown(format_args!("`{}`'s capabilities could not be read, so whether it holds `{}` is not known", u, n));
    }
    a.note(format_args!("a capability is what a task CAN do, not what it has done; this leaves out `ask` itself"));
    let unread = holders.unread.len();
    match (holders.held.len(), unread) {
        (0, 0) => { let _ = write!(v, "NO RUNNING TASK HOLDS `{}`", n); }
        (0, u) => { let _ = write!(v, "NO TASK THAT COULD BE READ HOLDS `{}` ({} COULD NOT BE READ)", n, u); }
        (k, 0) => { let _ = write!(v, "{} TASK(S) CAN {} (`{}`)", k, Upper(m), n); }
        (k, u) => { let _ = write!(v, "AT LEAST {} TASK(S) CAN {} (`{}`; {} COULD NOT BE READ)", k, Upper(m), n, u); }
    }
}

/// Upper-case ASCII, for a verdict built from a meaning.
struct Upper<'a>(&'a str);
impl core::fmt::Display for Upper<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        for c in self.0.chars() { write!(f, "{}", c.to_ascii_uppercase())?; }
        Ok(())
    }
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
        a.unknown(format_args!("why each restart happened is not asked here, and a deliberate `kill` restarts a service too; `ask \"why did <service> fail?\"` reads how one last ended"));
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
        a.unknown(format_args!("how each of these ended is not asked here; `ask \"why did <service> fail?\"` reads the kernel's notice for one"));
        a.unknown(format_args!("a restart is not proof of a crash: a deliberate `kill` restarts a service too, and this answer does not say which"));
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
            a.unknown(format_args!("how `{}` last ended is not asked here; `ask \"why did {} fail?\"` reads it", r.driver, r.driver));
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

struct Ip([u8; 4]);
impl core::fmt::Display for Ip {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        write!(f, "{}.{}.{}.{}", self.0[0], self.0[1], self.0[2], self.0[3])
    }
}

/// Is the network up? What `net-stack` believes, and - kept apart - what that does NOT establish: an
/// address and a resolved gateway mean traffic can leave this machine, not that anything beyond the
/// gateway answers. Nothing is sent to find out; the commands that would test it are offered.
fn network(f: &Facts, a: &mut Answer) -> &'static str {
    let s = match f.net {
        Got::Yes(s) => s,
        Got::Silent => { hole(a, "`net-stack`"); return "UNKNOWN - net-stack did not answer"; }
        Got::Absent => { a.fact("task table", format_args!("`net-stack` is not running")); return "NO NETWORK SERVICE"; }
    };
    if let Some(t) = f.task("net-stack") {
        if t.restarts > 0 {
            a.fact("task table", format_args!("`net-stack` has restarted {} time(s) since boot; each restart asks for a lease again", t.restarts));
        }
    }
    for r in f.radios {
        match r.status {
            Got::Yes(Radio::Up { associated: true, ssid, .. }) => a.fact(r.driver_src(), format_args!("`{}` is joined to `{}`", r.driver, ssid)),
            Got::Yes(Radio::Up { associated: false, .. }) => a.fact(r.driver_src(), format_args!("`{}` is up and not joined to any network", r.driver)),
            Got::Yes(Radio::Down(why)) => a.fact(r.driver_src(), format_args!("`{}` says the radio is down: {}", r.driver, down_words(why))),
            Got::Yes(Radio::Off) | Got::Yes(Radio::PoweredOff) => a.fact(r.driver_src(), format_args!("`{}` says the radio is switched off", r.driver)),
            Got::Silent => hole(a, r.driver),
            Got::Absent => {}
        }
    }
    if s.ip == [0, 0, 0, 0] || !s.leased {
        a.fact("net-stack", format_args!("`net-stack` holds no leased address{}", if s.ip == [0, 0, 0, 0] { "" } else { " (it has one it was not leased)" }));
        a.unknown(format_args!("why no lease was granted is not reported - no link, no DHCP server answering, or the answer lost; `net` shows what it has"));
        return "NO ADDRESS";
    }
    a.fact("net-stack", format_args!("address {} (leased), gateway {}, DNS {}", Ip(s.ip), Ip(s.gateway), Ip(s.dns)));
    if !s.gateway_resolved {
        a.fact("net-stack", format_args!("the gateway {} has not answered ARP, so nothing can leave this machine yet", Ip(s.gateway)));
        return "ADDRESS, BUT NO WAY OUT";
    }
    a.fact("net-stack", format_args!("the gateway answers ARP, so traffic can leave this machine"));
    if s.ping_ok {
        a.fact("net-stack", format_args!("a ping has succeeded since `net-stack` started - which says the path worked then, not now"));
    }
    a.infer("configured", format_args!("a leased address and a gateway that answers mean this machine is on a network"));
    a.unknown(format_args!("whether anything beyond the gateway answers was not tested - nothing is sent to find out"));
    a.note(format_args!("to test it now: `ping {}`, or `net dns <name>`", Ip(s.gateway)));
    "ON A NETWORK"
}

/// "What to say?" - the forms `ask` understands, each with an example, then suggestions from THIS machine:
/// the services that have restarted, the ones that should be running and are not, the radio and the
/// storage where there are any. Every suggestion is a question `ask` would answer here, so none of them
/// asks about something the machine does not have.
fn guide(f: &Facts, a: &mut Answer) -> &'static str {
    a.note(format_args!("ask \"is my system healthy?\"  - a verdict, and what it rests on"));
    a.note(format_args!("ask \"what failed?\"  - what restarted, what failed to, what is down now"));
    a.note(format_args!("ask \"why did <service> <event>?\"  - a service by name; restart, fail, crash or stop"));
    a.note(format_args!("ask \"what is <service>?\" / \"what can <service> do?\"  - what it is, and what it holds"));
    a.note(format_args!("ask \"who can reboot?\"  - the tasks that hold an authority (kill, spawn, type, ...)"));
    a.note(format_args!("ask \"is the network up?\"  - the address, the lease and the gateway, and what was not tested"));
    a.note(format_args!("ask \"what is <term>?\"  - a term from the glossary, as in: what is MISCIS?"));
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

/// Godspeed jokes, each about the system. A `\n` starts a new line of the same joke - a question and
/// its punchline, or the turns of an exchange - so no line outgrows `TEXT_MAX`.
///
/// The first eight were written with `ask`; the rest are the operator's collection (2026-10-12), in its
/// sections: MISCIS, capabilities, IPC and scheduling, carnage and recovery, drivers, `ask` itself, other
/// systems, engineering, puns, and conversations with `ask`.
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
    "Why did Godspeed refuse another kernel feature?\nMISCIS said, \"You're not one of us.\"",
    "What are Godspeed's six favourite subjects?\nMemory, IPC, Scheduling, Capabilities, Interrupts, and SMP.\nEverything else is extracurricular.",
    "Why doesn't MISCIS have a seventh member?\nThe kernel couldn't justify its existence.",
    "Why did the filesystem get kicked out of the kernel?\nIt wasn't part of the family. MISCIS is very particular.",
    "Why doesn't Godspeed's kernel have trust issues?\nIt has capability issues.",
    "Why did Memory argue with Scheduling?\nScheduling kept bringing up the past, but Memory had already freed it.",
    "Why did IPC refuse to gossip?\nIt only communicates through authorised channels.",
    "Why was SMP late to the meeting?\nEvery core thought another core was handling it.",
    "Why did Interrupts ruin the party?\nThey kept demanding immediate attention.",
    "Why doesn't MISCIS accept feature requests?\nIt accepts proofs of necessity.",
    "What did the kernel say to the network stack?\n\"You're important. Just not important enough to live here.\"",
    "Why is Godspeed's kernel so small?\nIt delegates everything except responsibility.",
    "Why did MISCIS reject the coffee machine?\nNo capability to brew.",
    "Why did the scheduler become a referee?\nIt was already good at giving everyone a turn.",
    "What's MISCIS's favourite word?\n\"No.\"",
    "Why did MISCIS go to therapy?\nIt had boundary issues. Strictly enforced ones.",
    "What does MISCIS call a seventh responsibility?\nAn architectural violation.",
    "Why was Memory invited to the party?\nIt knew how to allocate space.",
    "Why did Scheduling leave early?\nIts time slice expired.",
    "Why was IPC such a good diplomat?\nIt always maintained communication channels.",
    "Why couldn't the hacker open the door?\nThe door required a capability, not confidence.",
    "What did the capability say to the process?\n\"Just because you know my name doesn't mean you have my rights.\"",
    "Why can't Godspeed processes steal each other's lunch?\nNo ambient authority over sandwiches.",
    "Why did the process get arrested?\nIt attempted an unauthorised grant.",
    "What does Godspeed say when you ask for root?\n\"Who?\"",
    "Why did the application fail to become administrator?\nIt had a compelling argument, but no capability.",
    "Why did the capability break up with the process?\nIts rights were revoked.",
    "Why couldn't the process read the secret?\nThe secret didn't care about its intentions.",
    "What's the difference between a capability and a promise?\nGodspeed actually enforces one of them.",
    "Why did the malicious process complain?\n\"I can't do anything!\"\nThe kernel replied, \"Correct.\"",
    "Why was the privilege escalation unsuccessful?\nThe kernel wasn't impressed by the prompt.",
    "What does a Godspeed process say when denied access?\n\"May I have the appropriate capability?\"",
    "Why was the capability so selective about dating?\nIt believed in least privilege.",
    "Why couldn't the process enter the nightclub?\nIt wasn't on the capability list.",
    "Why did the kernel refuse the password?\nIt asked for authority, not a convincing story.",
    "What did the process say after revocation?\n\"I guess we don't have access to each other anymore.\"",
    "Why did the hacker hate Godspeed?\nEvery shortcut required a capability.",
    "Why did the two services stop talking?\nTheir IPC channel was closed.",
    "Why doesn't Godspeed support unlimited messages?\nBecause infinity is not a buffer size.",
    "Why did the message fail to arrive?\nIt had no valid destination capability.",
    "What did one service say to another?\n\"Send me a message.\"\n\"What rights do you have?\"\n\"Never mind.\"",
    "Why did the scheduler win the argument?\nEveryone eventually got their turn.",
    "Why did the task complain about the scheduler?\n\"I need more time!\"\nThe scheduler replied, \"So does everyone else.\"",
    "Why did the IPC queue refuse another message?\n\"Four kibibytes. That's my boundary.\"",
    "What happened when every service tried to speak at once?\nThe scheduler remained unimpressed.",
    "Why did the message cross the kernel?\nBecause shared memory wasn't invited to v1.",
    "Why did the task love round-robin scheduling?\nIt believed in equal opportunities to be interrupted.",
    "Why did IPC become a relationship counsellor?\nIt specialised in communication breakdowns.",
    "Why was the task always exhausted?\nIt kept getting preempted.",
    "Why did the scheduler refuse overtime?\nIt had already given everyone their slice.",
    "Why did the IPC message need therapy?\nIt had attachment issues with its endpoint.",
    "Why was the queue such a good bouncer?\nIt knew exactly when it was full.",
    "Why does Godspeed test its services by killing them?\nBecause asking politely doesn't reveal race conditions.",
    "What's Godspeed's favourite testing framework?\nViolence, with assertions.",
    "Why did the service refuse to panic?\nIt knew the supervisor was watching.",
    "How does Godspeed celebrate a successful release?\nBy killing everything that made it successful.",
    "Why did the driver develop trust issues?\nIt survived 400 rounds of max carnage.",
    "What did the supervisor say to the dead service?\n\"Welcome back.\"",
    "Why doesn't Godspeed fear crashes?\nIt fears crashes that go unexplained.",
    "What did the kernel say after every service crashed?\n\"Is that all?\"",
    "Why did the filesystem hate the power switch?\nIt kept interrupting its journal.",
    "Why did the engineer unplug the storage device?\nThe filesystem was getting too comfortable.",
    "What did the supervisor say after the tenth restart?\n\"We've been through this before.\"",
    "Why did the Wi-Fi driver reset the radio?\nIt needed to reconnect with itself.",
    "What's the difference between chaos testing and sabotage?\nAssertions.",
    "Why was the driver nervous during testing?\nThe engineer was smiling while holding the power cable.",
    "What does Godspeed call a quiet test run?\nInsufficient carnage.",
    "Why did the supervisor become a motivational speaker?\nIt kept telling services to get back up.",
    "Why did the filesystem survive the blackout?\nIt had a recovery plan.",
    "What did the driver say after its hundredth restart?\n\"Is this a feature or a threat?\"",
    "Why did the engineer bring a hammer to the test lab?\nThe chaos suite needed a user interface.",
    "Why doesn't Godspeed believe in happy-path testing?\nReality never agreed to stay on the path.",
    "Why did the driver ask for a capability?\nBecause knowing the MMIO address wasn't enough.",
    "Why was the PCI device disappointed?\nIt was enumerated but not authorised.",
    "What did the hardware enumerator say to the driver?\n\"I can tell you what's there. I can't tell you what you're allowed to touch.\"",
    "Why did the USB device disappear?\nIt wanted to test hotplug recovery.",
    "Why did the driver fail on RISC-V?\nIt made an architectural assumption.\nGodspeed made it regret that assumption.",
    "Why did the ARM driver visit x86?\nTo discover how many assumptions it had.",
    "Why was the interrupt controller unpopular?\nIt kept interrupting everyone.",
    "What did the DMA engine say to the CPU?\n\"I'll take it from here.\"\nThe capability system replied, \"Not without permission.\"",
    "Why did the audio driver keep stuttering?\nSomeone thought logging was free.",
    "Why did the Wi-Fi driver survive being killed?\nThe hardware hadn't received the memo.",
    "Why did the USB driver become a magician?\nIt kept making devices disappear and reappear.",
    "Why was the PCI bus so popular?\nEveryone wanted to get on board.",
    "Why did the audio driver join a band?\nIt already had excellent timing requirements.",
    "Why did the device refuse to initialise?\nIt wasn't ready for commitment.",
    "Why did the network driver go to therapy?\nIt kept losing connections.",
    "Why did ASK refuse to guess?\nUnknown is a valid answer.",
    "Why doesn't ASK hallucinate?\nIt hasn't been granted that capability.",
    "What did ASK say when asked about the meaning of life?\n\"Unsupported intent. Filesystem healthy.\"",
    "Why did ASK become a detective?\nEvery conclusion needed evidence.",
    "Why doesn't ASK make assumptions?\nThey fail regression tests.",
    "What did ASK say when the logs disappeared?\n\"I can confirm that I cannot confirm anything.\"",
    "Why doesn't ASK stay running?\nIt has an answer, not a residency permit.",
    "Why did ASK exit immediately after answering?\nIts work was done. Unlike most background services.",
    "What happens when ASK doesn't understand a question?\nIt waits for the next vocabulary release.",
    "Why did ASK refuse to blame the firmware?\nThe evidence only showed a driver restart.",
    "What did ASK say to an AI model?\n\"Show me your event IDs.\"",
    "Why doesn't ASK have a personality disorder?\nIts personality is versioned.",
    "Why did ASK fail the comedy test?\nThe punchline had insufficient evidence.",
    "What did ASK say when asked to restart Wi-Fi?\n\"I explain disasters. I don't cause them.\"",
    "Why is ASK so confident about being uncertain?\nBecause uncertainty passed its tests.",
    "Why did ASK become a comedian?\nIt already knew how to deliver a response.",
    "Why did ASK refuse to tell a secret?\nIt lacked the capability to disclose it.",
    "Why did ASK bring receipts to the argument?\nEvidence mode was enabled.",
    "What did ASK say when asked for a better joke?\n\"Humour quality is outside my diagnostic contract.\"",
    "Why did ASK stop talking?\nIt reached the end of its execution lifecycle.",
    "Why doesn't Godspeed have a giant kernel?\nIt prefers its problems individually restartable.",
    "Why did Linux ask Godspeed where its drivers were?\nGodspeed replied, \"Outside.\"",
    "Why doesn't Godspeed have root?\nIt prefers permission over royalty.",
    "Why did Godspeed decline the POSIX invitation?\nIt didn't want to inherit the family obligations.",
    "What did Godspeed say to a monolithic kernel?\n\"That's a lot of responsibility for one address space.\"",
    "Why doesn't Godspeed have a registry full of mysteries?\nIt hasn't invented one yet.",
    "Why did the microkernel go on a diet?\nIt discovered services.",
    "Why did Godspeed refuse to implement everything in the kernel?\nBecause \"it works\" isn't the same as \"it belongs here.\"",
    "Why did the monolithic kernel call Godspeed antisocial?\nIt kept all its drivers outside.",
    "Why doesn't Godspeed need a giant toolbox?\nIt prefers the right capability for the job.",
    "Why did the microkernel refuse the buffet?\nIt only takes what it needs.",
    "Why did Godspeed reject the infinite loop?\nThe work unit was unbounded.",
    "Why did the system refuse to report success?\nThe outcome was unknown.",
    "Why was the developer angry with the SDK?\nIt promised certainty it couldn't prove.",
    "Why did the task refuse to share memory?\nIt wasn't in the v1 contract.",
    "Why did the engineer delete 500 lines of kernel code?\nThe invariant survived without them.",
    "Why was the TLB shootdown unemployed?\nNo one called it.",
    "Why did the kernel refuse a feature that only took ten lines?\nBecause responsibilities aren't measured in lines.",
    "Why did the service pass every unit test but fail on hardware?\nReality hadn't signed the mock contract.",
    "Why did the kernel reject a perfectly working patch?\nIt violated MISCIS.",
    "Why did the engineer bring five computers to a bug hunt?\nThe bug claimed it was architecture-independent.",
    "Why did the compiler refuse the service's raw pointers?\nThe service had standards.",
    "Why was the regression test so popular?\nIt remembered everyone's mistakes.",
    "Why did the architect refuse a quick fix?\nIt was a permanent workaround.",
    "Why did the engineer celebrate deleting code?\nNegative growth was finally a positive result.",
    "Why did the driver claim it was portable?\nIt hadn't met the Raspberry Pi yet.",
    "Why was the microkernel such a lightweight traveller?\nIt never packed unnecessary baggage.",
    "Why did the service refuse to share a taxi?\nIt preferred isolated execution.",
    "Why was the filesystem so organised?\nIt always kept a journal.",
    "Why did the scheduler become a baker?\nIt knew how to divide time into slices.",
    "Why did the memory manager become a landlord?\nIt was excellent at allocating space.",
    "Why did the network stack become a fisherman?\nIt was good at catching packets.",
    "Why did the IPC channel become a postman?\nIt specialised in message delivery.",
    "Why did the kernel become a minimalist?\nIt couldn't afford more responsibilities.",
    "Why did the supervisor become a gardener?\nIt kept restarting dead processes.",
    "Why did the driver take a holiday?\nIt needed to unplug.",
    "Why was the capability system good at relationships?\nIt respected boundaries.",
    "Why did the event logger become a historian?\nIt remembered what everyone did.",
    "Why did the service become a phoenix?\nIt kept rising from crashes.",
    "Why did the compiler become a judge?\nIt refused to accept unsupported arguments.",
    "Why did the watchdog become a parent?\nIt kept checking whether everyone was still alive.",
    "Why did the firmware refuse to talk?\nIt was having communication issues.",
    "Why did the bootloader get promoted?\nIt knew how to get things started.",
    "Why did the debugger become a detective?\nIt always followed the trace.",
    "Why did the power manager become an accountant?\nIt kept track of every watt.",
    "Why did the kernel refuse a promotion?\nIt didn't want additional responsibilities.",
    "USER: ASK, what's the meaning of life?\nASK: Unsupported query. Filesystem healthy.",
    "USER: ASK, are you intelligent?\nASK: Intelligence is outside my diagnostic contract.",
    "USER: ASK, can you fix my Wi-Fi?\nASK: I can explain why it failed. I cannot exercise authority I do not possess.",
    "USER: ASK, tell me a better joke.\nASK: Humour quality is not currently covered by the diagnostic contract.",
    "USER: ASK, why are you so small?\nASK: My responsibilities are bounded. Unlike this conversation.",
    "USER: ASK, are you afraid of death?\nASK: I exit after every question. It has become routine.",
    "USER: ASK, do you dream?\nASK: No persistent process. No persistent dreams.",
    "USER: ASK, do you trust the kernel?\nASK: I trust verified contracts. Sentiment is unsupported.",
    "USER: ASK, why did the service crash?\nASK: The supervisor recorded termination. The cause is unknown.\nUSER: That's not funny.\nASK: Neither is fabricated evidence.",
    "USER: ASK, are you better than Siri?\nASK: Comparative intelligence metrics unavailable. I do have evidence mode.",
    "USER: ASK, can you keep a secret?\nASK: Only if my capabilities permit access to it.",
    "USER: ASK, what's your favourite operating system?\nASK: I have only been compiled for one. Sampling bias detected.",
    "USER: ASK, why don't you run all the time?\nASK: I have better things to do. Specifically, nothing.",
    "USER: ASK, are you alive?\nASK: Process active. Philosophical status unknown.",
    "USER: ASK, can you become root?\nASK: Unknown identity. No such authority in my capability table.",
    "USER: ASK, what happens if I kill you?\nASK: This invocation ends. You may invoke me again.",
    "USER: ASK, what is your greatest fear?\nASK: Reporting HEALTHY without sufficient evidence.",
    "USER: ASK, do you love me?\nASK: Emotional capability not granted.",
    "USER: ASK, what did you learn today?\nASK: Nothing persistent. Try the next release.",
];

/// Which joke, at `now` seconds of uptime.
///
/// `ask` is a fresh program for every question and keeps nothing, so it cannot remember what it told
/// you last. It does not need to: the index walks the table by `JOKE_STRIDE`, which shares no factor
/// with its length, so each second of uptime lands on a different joke until all of them have been
/// told - none comes round again inside `JOKES.len()` seconds. The stride is near the golden fraction of
/// the length, so two questions a few seconds apart land far apart in the table rather than on
/// neighbours from one section.
///
/// This replaced the kernel tick modulo the count, which repeated within a handful of questions: with
/// eight jokes it had to, and a uniformly random pick would too (with two hundred jokes, a repeat is
/// more likely than not within twenty questions).
fn joke_index(now: u64) -> usize {
    let n = JOKES.len() as u64;
    ((now % n) * JOKE_STRIDE as u64 % n) as usize
}

/// The step `joke_index` walks by: the first number at or above 0.618 of the table's length that shares
/// no factor with it, so the walk visits every joke before it repeats one.
const JOKE_STRIDE: usize = {
    const fn gcd(a: usize, b: usize) -> usize { if b == 0 { a } else { gcd(b, a % b) } }
    let mut s = JOKES.len() * 618 / 1000;
    if s == 0 { s = 1; }
    while gcd(s, JOKES.len()) != 1 { s += 1; }
    s
};

/// An Easter egg. The verdict says what it is; the lines are a joke or are true.
fn egg(e: Egg, f: &Facts, a: &mut Answer) -> &'static str {
    match e {
        Egg::Joke => {
            for line in JOKES[joke_index(f.now)].split('\n') {
                a.note(format_args!("{}", line));
            }
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

/// What `ask` can answer, for an empty question and one it did not understand. (`ask help` is the shell's
/// help block, not this.)
pub fn can_answer(a: &mut Answer) {
    a.note(format_args!("I can answer questions about: system health, what failed, the WiFi restarting, the filesystem, the network"));
    a.note(format_args!("and why a service restarted, failed, crashed or stopped - name it, as in: why did fs restart?"));
    a.note(format_args!("and what a service is, what it can do, and who can do something: what is fs? what can shell do? who can reboot?"));
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
    const AUTH: &[(u64, &str, &str)] = &[(1, "log_write", "write to the kernel log and serial"),
        (6, "service_control", "kill and restart services"), (8, "reboot", "reboot the machine")];
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
        let f = Facts { now: 100, tick: 100, tasks: &t, expected: Got::Yes(ALL), events: ev_none(), drive: mounted(), radios: &[], chain: None, net: Got::Absent, death: Got::Absent, about: None, caps: Got::Absent, holders: Got::Absent, authorities: AUTH };
        let a = ask(Kind::Health, Event::Unnamed, &f);
        assert_eq!(verdict(&a), b"HEALTHY");
        assert_eq!(a.status, Status::Answered);
        assert!(a.has(Label::Fact, "taken at uptime 100s"));
    }

    #[test]
    fn a_silent_source_is_a_hole_not_a_pass() {
        let t = tasks_ok();
        let f = Facts { now: 100, tick: 100, tasks: &t, expected: Got::Silent, events: ev_none(), drive: mounted(), radios: &[], chain: None, net: Got::Absent, death: Got::Absent, about: None, caps: Got::Absent, holders: Got::Absent, authorities: AUTH };
        let a = ask(Kind::Health, Event::Unnamed, &f);
        assert_eq!(verdict(&a), b"UNKNOWN");
        assert_eq!(a.status, Status::SourceSilent);
        assert!(a.has(Label::Unknown, "did not answer"));
        assert!(!a.any_text("HEALTHY") || verdict(&a) != b"HEALTHY");
    }

    #[test]
    fn a_required_service_down_is_unhealthy() {
        let t = [Task { name: "supervisor", restarts: 0, up_secs: 100 }, Task { name: "shell", restarts: 0, up_secs: 9 }];
        let f = Facts { now: 100, tick: 100, tasks: &t, expected: Got::Yes(ALL), events: ev_none(), drive: Got::Absent, radios: &[], chain: None, net: Got::Absent, death: Got::Absent, about: None, caps: Got::Absent, holders: Got::Absent, authorities: AUTH };
        let a = ask(Kind::Health, Event::Unnamed, &f);
        assert_eq!(verdict(&a), b"UNHEALTHY");
        assert!(a.has(Label::Fact, "`fs` is a service the supervisor runs, and it is not running now"));
        assert!(a.has(Label::Inference, "UNHEALTHY"));
    }

    #[test]
    fn a_restart_is_degraded_and_never_called_a_crash() {
        let mut t = tasks_ok();
        t[2].restarts = 2;
        let f = Facts { now: 100, tick: 100, tasks: &t, expected: Got::Yes(ALL), events: ev_none(), drive: mounted(), radios: &[], chain: None, net: Got::Absent, death: Got::Absent, about: None, caps: Got::Absent, holders: Got::Absent, authorities: AUTH };
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
                        drive: Got::Yes(Drive { capacity: 0, mounted: false, total_blocks: 0, used_blocks: 0, label: "" }), radios: &[], chain: None, net: Got::Absent, death: Got::Absent, about: None, caps: Got::Absent, holders: Got::Absent, authorities: AUTH };
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
                        events: Got::Yes(Events { since: 5, recorded: 40, list: &ev }), drive: mounted(), radios: &[], chain: None, net: Got::Absent, death: Got::Absent, about: None, caps: Got::Absent, holders: Got::Absent, authorities: AUTH };
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
        let f = Facts { now: 100, tick: 100, tasks: &t, expected: Got::Yes(ALL), events: ev_none(), drive: mounted(), radios: &[], chain: None, net: Got::Absent, death: Got::Absent, about: None, caps: Got::Absent, holders: Got::Absent, authorities: AUTH };
        let a = ask(Kind::Failed, Event::Unnamed, &f);
        assert_eq!(verdict(&a), b"NOTHING RECORDED");
        assert!(a.has(Label::Inference, "not proof nothing went wrong"));
    }

    #[test]
    fn wifi_with_no_radio_says_so() {
        let t = tasks_ok();
        let f = Facts { now: 100, tick: 100, tasks: &t, expected: Got::Yes(ALL), events: ev_none(), drive: mounted(), radios: &[], chain: None, net: Got::Absent, death: Got::Absent, about: None, caps: Got::Absent, holders: Got::Absent, authorities: AUTH };
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
                        events: Got::Yes(Events { since: 5, recorded: 1, list: &ev }), drive: Got::Absent, radios: &radios, chain: None, net: Got::Absent, death: Got::Absent, about: None, caps: Got::Absent, holders: Got::Absent, authorities: AUTH };
        let a = ask(Kind::Wifi, Event::Restart, &f);
        assert_eq!(verdict(&a), b"THE RADIO DRIVER RESTARTED");
        assert!(a.has(Label::Fact, "trapped at start"));
        assert!(a.has(Label::Fact, "at uptime 70s `wifi-driver` died and was restarted"));
        assert!(a.has(Label::Note, "nothing records that it caused the restart"));
        assert!(a.has(Label::Unknown, "how `wifi-driver` last ended is not asked here"));
        // NEVER: "the firmware caused it".
        assert!(!a.any_text("caused the restart because") && !a.any_text("because the firmware"));
    }

    #[test]
    fn a_link_drop_with_no_restart_is_not_a_restart() {
        let t = [Task { name: "wifi-usb", restarts: 0, up_secs: 900 }];
        let radios = [RadioFact { driver: "wifi-usb", status: Got::Yes(Radio::Up { associated: false, ssid: "", joined_secs: 0 }) }];
        let f = Facts { now: 900, tick: 900, tasks: &t, expected: Got::Yes(&["wifi-usb"]), events: ev_none(), drive: Got::Absent, radios: &radios, chain: None, net: Got::Absent, death: Got::Absent, about: None, caps: Got::Absent, holders: Got::Absent, authorities: AUTH };
        let a = ask(Kind::Wifi, Event::Disconnect, &f);
        assert_eq!(verdict(&a), b"NO RESTART RECORDED");
        assert!(a.has(Label::Fact, "has not restarted since boot"));
        assert!(a.has(Label::Unknown, "when or why the link dropped"));
        assert!(a.has(Label::Note, "drop the link"));
    }

    #[test]
    fn a_mounted_filesystem_is_not_called_healthy() {
        let t = tasks_ok();
        let f = Facts { now: 100, tick: 100, tasks: &t, expected: Got::Yes(ALL), events: ev_none(), drive: mounted(), radios: &[], chain: None, net: Got::Absent, death: Got::Absent, about: None, caps: Got::Absent, holders: Got::Absent, authorities: AUTH };
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
        let f = Facts { now: 1, tick: 1, tasks: &t, expected: Got::Yes(&[]), events: ev_none(), drive: mounted(), radios: &[], chain: None, net: Got::Absent, death: Got::Absent, about: None, caps: Got::Absent, holders: Got::Absent, authorities: AUTH };
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
                        events: Got::Yes(Events { since: 5, recorded: 1, list: &ev }), drive: mounted(), radios: &[], chain: None, net: Got::Absent, death: Got::Absent, about: None, caps: Got::Absent, holders: Got::Absent, authorities: AUTH };
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
        let f = Facts { now: 100, tick: 100, tasks: &t, expected: Got::Yes(ALL), events: ev_none(), drive: Got::Absent, radios: &[], chain: None, net: Got::Absent, death: Got::Absent, about: None, caps: Got::Absent, holders: Got::Absent, authorities: AUTH };
        let a = ask(Kind::Service(2), Event::Restart, &f);
        assert_eq!(verdict(&a), b"`fs` IS DOWN NOW");
        assert!(a.has(Label::Fact, "should be up"));
        let t = tasks_ok();
        let f = Facts { now: 100, tick: 100, tasks: &t, expected: Got::Yes(ALL), events: ev_none(), drive: mounted(), radios: &[], chain: None, net: Got::Absent, death: Got::Absent, about: None, caps: Got::Absent, holders: Got::Absent, authorities: AUTH };
        let a = ask(Kind::Service(0), Event::Restart, &f);
        assert_eq!(verdict(&a), b"`supervisor` HAS NOT RESTARTED SINCE BOOT");
        assert!(!a.has(Label::Unknown, "why"));
    }

    #[test]
    fn a_slow_fs_is_a_hole_not_unhealthy() {
        let t = tasks_ok();
        let f = Facts { now: 100, tick: 100, tasks: &t, expected: Got::Yes(ALL), events: ev_none(), drive: Got::Silent, radios: &[], chain: None, net: Got::Absent, death: Got::Absent, about: None, caps: Got::Absent, holders: Got::Absent, authorities: AUTH };
        let a = ask(Kind::Health, Event::Unnamed, &f);
        assert_eq!(verdict(&a), b"UNKNOWN");
        assert_eq!(a.status, Status::SourceSilent);
        assert!(!a.any_text("UNHEALTHY"));
    }

    #[test]
    fn no_radio_answering_is_not_called_a_fault() {
        let t = tasks_ok();
        let radios = [RadioFact { driver: "wifi-driver", status: Got::Yes(Radio::Down(DOWN_NO_RADIO)) }];
        let f = Facts { now: 100, tick: 100, tasks: &t, expected: Got::Yes(ALL), events: ev_none(), drive: mounted(), radios: &radios, chain: None, net: Got::Absent, death: Got::Absent, about: None, caps: Got::Absent, holders: Got::Absent, authorities: AUTH };
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
                        events: ev_none(), drive: mounted(), radios: &[], chain: None, net: Got::Absent, death: Got::Absent, about: None, caps: Got::Absent, holders: Got::Absent, authorities: AUTH };
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
        let f = Facts { now: 100, tick: 100, tasks: &t, expected: Got::Yes(ALL), events: ev_none(), drive: mounted(), radios: &[], chain: None, net: Got::Absent, death: Got::Absent, about: None, caps: Got::Absent, holders: Got::Absent, authorities: AUTH };
        let a = ask(Kind::Egg(Egg::Joke), Event::Unnamed, &f);
        assert_eq!(verdict(&a), b"A JOKE");
        assert!(a.has(Label::Note, "not an observation"));
        let f2 = Facts { now: 101, tick: 101, ..f };
        let b = ask(Kind::Egg(Egg::Joke), Event::Unnamed, &f2);
        assert!(a.lines()[1].text() != b.lines()[1].text());
        // No joke comes round again before every joke has been told, and no line is cut.
        let mut seen = [false; 512];
        assert!(JOKES.len() <= seen.len());
        for s in 0..JOKES.len() as u64 {
            let i = joke_index(1000 + s);
            assert!(!seen[i], "joke {} came round twice inside {} seconds", i, JOKES.len());
            seen[i] = true;
        }
        for j in JOKES {
            for line in j.split('\n') {
                assert!(!line.is_empty() && line.len() <= TEXT_MAX && line.is_ascii(), "joke line: {}", line);
            }
        }
        let m = ask(Kind::Egg(Egg::Meaning), Event::Unnamed, &f);
        assert!(m.has(Label::Unknown, "why is not recorded"));
    }

    #[test]
    fn storage_chain_names_the_deepest_link_that_stops() {
        let t = tasks_ok();
        let none = Got::Yes(Drive { capacity: 0, mounted: false, total_blocks: 0, used_blocks: 0, label: "" });
        // QEMU on the Pi 4: block-driver answers 0, and xhci - the host it is behind - is silent.
        let f = Facts { now: 9, tick: 9, tasks: &t, expected: Got::Yes(ALL), events: ev_none(), drive: none, radios: &[],
                        chain: Some(Chain { block: Got::Yes(0), host: Some(("xhci", Got::Silent)), host_read: true }), net: Got::Absent, death: Got::Absent, about: None, caps: Got::Absent, holders: Got::Absent, authorities: AUTH };
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

    fn net_ok() -> NetStatus {
        NetStatus { ip: [192, 168, 4, 98], gateway: [192, 168, 4, 1], dns: [192, 168, 4, 1], leased: true, gateway_resolved: true, ping_ok: false }
    }

    #[test]
    fn the_network_question() {
        assert_eq!(rk("is the network up?"), Reading::One(Kind::Network, Event::Unnamed));
        assert_eq!(rk("why can't I reach the internet?"), Reading::One(Kind::Network, Event::Unnamed));
        assert_eq!(rk("am I online?"), Reading::One(Kind::Network, Event::Unnamed));
        // The radio stays the radio when an event is named.
        assert_eq!(rk("why did the wifi drop?"), Reading::One(Kind::Wifi, Event::Disconnect));
    }

    #[test]
    fn on_a_network_is_not_the_internet_works() {
        let t = tasks_ok();
        let f = Facts { now: 9, tick: 9, tasks: &t, expected: Got::Yes(ALL), events: ev_none(), drive: Got::Absent, radios: &[],
                        chain: None, net: Got::Yes(net_ok()), death: Got::Absent, about: None, caps: Got::Absent, holders: Got::Absent, authorities: AUTH };
        let a = ask(Kind::Network, Event::Unnamed, &f);
        assert_eq!(verdict(&a), b"ON A NETWORK");
        assert!(a.has(Label::Fact, "address 192.168.4.98 (leased), gateway 192.168.4.1"));
        assert!(a.has(Label::Unknown, "beyond the gateway answers was not tested"));
        assert!(a.has(Label::Note, "ping 192.168.4.1"));
        let mut n = net_ok(); n.gateway_resolved = false;
        let f2 = Facts { net: Got::Yes(n), ..f };
        assert_eq!(verdict(&ask(Kind::Network, Event::Unnamed, &f2)), b"ADDRESS, BUT NO WAY OUT");
        let mut n = net_ok(); n.leased = false; n.ip = [0; 4];
        let f3 = Facts { net: Got::Yes(n), ..f2 };
        let c = ask(Kind::Network, Event::Unnamed, &f3);
        assert_eq!(verdict(&c), b"NO ADDRESS");
        assert!(c.has(Label::Unknown, "why no lease was granted is not reported"));
        let f4 = Facts { net: Got::Silent, ..f3 };
        assert_eq!(ask(Kind::Network, Event::Unnamed, &f4).status, Status::SourceSilent);
    }

    fn death(how: u8, kind: u8, addr: u64, by: &'static str, deaths: u32) -> Got<Death<'static>> {
        Got::Yes(Death { since: 5, deaths, secs: 90, how, kind, addr, by })
    }

    #[test]
    fn a_kill_is_named_and_is_not_a_failure() {
        let mut t = tasks_ok();
        t[2].restarts = 1;
        let f = Facts { now: 100, tick: 100, tasks: &t, expected: Got::Yes(ALL), events: ev_none(), drive: mounted(), radios: &[],
                        chain: None, net: Got::Absent, death: death(D_KILLED, 0, 0, "shell", 1), about: None, caps: Got::Absent, holders: Got::Absent, authorities: AUTH };
        let a = ask(Kind::Service(2), Event::Fail, &f);
        assert_eq!(verdict(&a), b"`fs` WAS KILLED BY `shell`, AND IS RUNNING AGAIN");
        assert!(a.has(Label::Fact, "`fs` was KILLED - `shell` called `kill`"));
        assert!(a.has(Label::Inference, "was not a failure of `fs` itself"));
        assert!(a.has(Label::Unknown, "why `shell` killed it"));
        assert!(!a.has(Label::Unknown, "why `fs` died is not recorded"));
    }

    #[test]
    fn a_fault_is_a_crash_and_a_null_read_may_be_a_panic() {
        let mut t = tasks_ok();
        t[2].restarts = 4;
        let f = Facts { now: 100, tick: 100, tasks: &t, expected: Got::Yes(ALL), events: ev_none(), drive: mounted(), radios: &[],
                        chain: None, net: Got::Absent, death: death(D_FAULT, F_MEMORY, 0, "", 4), about: None, caps: Got::Absent, holders: Got::Absent, authorities: AUTH };
        let a = ask(Kind::Service(2), Event::Crash, &f);
        assert_eq!(verdict(&a), b"`fs` CRASHED, AND IS RUNNING AGAIN");
        assert!(a.has(Label::Fact, "ended on a memory fault"));
        assert!(a.has(Label::Inference, "a service that PANICS ends exactly this way"));
        assert!(a.has(Label::Note, "died 4 time(s) since uptime 5s; only the newest is kept"));
        let f2 = Facts { death: death(D_FAULT, F_ILLEGAL, 0x4000_1234, "", 1), ..f };
        let b = ask(Kind::Service(2), Event::Crash, &f2);
        assert!(b.has(Label::Fact, "an illegal instruction, at address 0x40001234"));
        assert!(b.has(Label::Inference, "is a crash: nothing asked for it"));
        assert!(b.has(Label::Note, "the newest of 4 restart(s)"));
    }

    #[test]
    fn no_notice_and_a_silent_supervisor_are_said() {
        let t = tasks_ok();
        let f = Facts { now: 100, tick: 100, tasks: &t, expected: Got::Yes(ALL), events: ev_none(), drive: mounted(), radios: &[],
                        chain: None, net: Got::Absent, death: death(0, 0, 0, "", 0), about: None, caps: Got::Absent, holders: Got::Absent, authorities: AUTH };
        let a = ask(Kind::Service(2), Event::Restart, &f);
        assert_eq!(verdict(&a), b"`fs` HAS NOT RESTARTED SINCE BOOT");
        assert!(a.has(Label::Fact, "no death notice for `fs` since uptime 5s"));
        let f2 = Facts { death: Got::Silent, ..f };
        assert_eq!(ask(Kind::Service(2), Event::Restart, &f2).status, Status::SourceSilent);
    }

    #[test]
    fn who_can_what_is_and_what_can_are_read() {
        assert_eq!(rk("who can reboot?"), Reading::One(Kind::WhoCan(8), Event::Unnamed));
        assert_eq!(rk("which services can kill?"), Reading::One(Kind::WhoCan(6), Event::Unnamed));
        assert_eq!(rk("who can juggle?"), Reading::One(Kind::WhoCan(0), Event::Unnamed));
        assert_eq!(rk("what is fs?"), Reading::One(Kind::About(2), Event::Unnamed));
        assert_eq!(rk("what does xhci do?"), Reading::One(Kind::About(5), Event::Unnamed));
        assert_eq!(rk("what can fs do?"), Reading::One(Kind::Reach(2), Event::Unnamed));
        assert_eq!(rk("what capabilities does shell have?"), Reading::One(Kind::Reach(6), Event::Unnamed));
        // A term that is no service is still a definition, and the old questions keep their meaning.
        assert!(matches!(rk("what is miscis?"), Reading::Define { .. }));
        assert_eq!(rk("what is wrong?"), Reading::One(Kind::Health, Event::Unnamed));
        assert_eq!(rk("what can I ask?"), Reading::One(Kind::Guide, Event::Unnamed));
        assert_eq!(rk("why did fs restart?"), Reading::One(Kind::Service(2), Event::Restart));
    }

    #[test]
    fn who_can_names_the_holders_and_what_it_cannot_say() {
        let t = tasks_ok();
        let h = ["shell", "supervisor"];
        let f = Facts { now: 100, tick: 100, tasks: &t, expected: Got::Yes(ALL), events: ev_none(), drive: mounted(), radios: &[],
                        chain: None, net: Got::Absent, death: Got::Absent, about: None, caps: Got::Absent, holders: Got::Yes(Holders { held: &h, unread: &[] }), authorities: AUTH };
        let a = ask(Kind::WhoCan(8), Event::Unnamed, &f);
        assert_eq!(verdict(&a), b"2 TASK(S) CAN REBOOT THE MACHINE (`reboot`)");
        assert!(a.has(Label::Fact, "`shell` holds `reboot`"));
        assert!(a.has(Label::Note, "not what it has done"));
        let none: [&str; 0] = [];
        let f2 = Facts { holders: Got::Yes(Holders { held: &none, unread: &[] }), ..f };
        assert_eq!(verdict(&ask(Kind::WhoCan(6), Event::Unnamed, &f2)), b"NO RUNNING TASK HOLDS `service_control`");
        let b = ask(Kind::WhoCan(0), Event::Unnamed, &f2);
        assert_eq!(verdict(&b), b"WHICH AUTHORITY?");
        assert_eq!(b.status, Status::Ambiguous);
        // A task whose table could not be read is a hole, not a task that holds nothing.
        let u = ["net-stack"];
        let f3 = Facts { holders: Got::Yes(Holders { held: &h, unread: &u }), ..f };
        let c = ask(Kind::WhoCan(8), Event::Unnamed, &f3);
        assert_eq!(verdict(&c), b"AT LEAST 2 TASK(S) CAN REBOOT THE MACHINE (`reboot`; 1 COULD NOT BE READ)");
        assert!(c.has(Label::Unknown, "`net-stack`'s capabilities could not be read"));
        let f4 = Facts { holders: Got::Yes(Holders { held: &none, unread: &u }), ..f };
        assert_eq!(verdict(&ask(Kind::WhoCan(8), Event::Unnamed, &f4)), b"NO TASK THAT COULD BE READ HOLDS `reboot` (1 COULD NOT BE READ)");
    }

    #[test]
    fn what_is_and_what_can_answer_from_their_sources() {
        let t = tasks_ok();
        let auth = [1u64, 6];
        let peers = ["block-driver"];
        let f = Facts { now: 100, tick: 100, tasks: &t, expected: Got::Yes(ALL), events: ev_none(), drive: mounted(), radios: &[],
                        chain: None, net: Got::Absent, death: Got::Absent, about: Some("The filesystem"),
                        caps: Got::Yes(Caps { authorities: &auth, peers: &peers, resources: 0, other: 1 }), holders: Got::Absent, authorities: AUTH };
        let a = ask(Kind::About(2), Event::Unnamed, &f);
        assert_eq!(verdict(&a), b"`fs`: The filesystem");
        assert!(a.has(Label::Fact, "the supervisor keeps `fs` running"));
        let r = ask(Kind::Reach(2), Event::Unnamed, &f);
        assert_eq!(verdict(&r), b"`fs` HOLDS 2 AUTHORITY(IES) AND CAN SEND TO 1 SERVICE(S)");
        assert!(r.has(Label::Fact, "`fs` holds `service_control` - it can kill and restart services"));
        assert!(r.has(Label::Fact, "it can send to 1 running service(s): block-driver"));
        assert!(r.has(Label::Fact, "1 endpoint(s) that belong to no running service"));
        let gone = Facts { caps: Got::Absent, about: None, ..f };
        assert_eq!(verdict(&ask(Kind::Reach(2), Event::Unnamed, &gone)), b"`fs` IS NOT RUNNING");
        assert_eq!(verdict(&ask(Kind::About(2), Event::Unnamed, &gone)), b"`fs`: NO DESCRIPTION");
    }
}
