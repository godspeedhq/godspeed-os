// SPDX-License-Identifier: Apache-2.0
//! Structured records - the typed `Table` that flows through GodspeedOS pipes.
//!
//! POSIX pipes carry text, so structured data is flattened to a string and `grep`/`awk`/`cut`
//! re-parse it back. GodspeedOS keeps the structure: a producer emits a **`Table`** (typed
//! columns + rows); filters operate on real fields (`where mem>0`), and JSON/YAML/the grid are
//! *renderings* of the model, not the model itself. This module is that model, lifted into the
//! SDK so **any** service - not just the shell - can build records, filter them, and render them
//! to JSON/YAML. See `docs/records.md`.
//!
//! ## Building a pipe-friendly service
//!
//! A service participates in a record pipe today without any new kernel surface: build a
//! `Table`, render it with [`Table::to_json`], and emit the bytes (EOT-terminated) like any byte
//! producer (`docs/pipes.md`). The shell's `| from json` lifts it back to records:
//!
//! ```ignore
//! let mut t = Table::new(&["name", "n"]);
//! let n = t.intern(b"alpha");
//! t.add_row(&[n, Value::Int(1)]);
//! let mut buf = MyBuf::new();        // any RecordSink (e.g. wrapping an IPC message)
//! t.to_json(&mut buf);              // → `[ {"name": "alpha", "n": 1} ]`
//! ```
//!
//! To cross a **service** boundary *as records* (skipping the JSON round-trip), use the bounded
//! binary **wire codec** instead: [`Table::encode`] on the producer, [`Table::decode`] on the
//! consumer. It is the `Table` itself on the wire - compact and typed, not JSON. `examples/roster`
//! produces records this way; the shell decodes them straight into a `Table`.
//!
//! ## Bounds (§26.6)
//!
//! Everything is fixed-size and stack-resident - no heap, loud on overflow: at most
//! [`REC_MAX_COLS`] columns, [`REC_MAX_ROWS`] rows, a [`REC_ARENA`]-byte string arena, and
//! [`REC_COL_NAME`]-byte column names. Overflowing any bound sets [`Table::overflow`].

use core::cmp::Ordering;

/// Maximum columns in a table.
pub const REC_MAX_COLS: usize = 8;
/// Maximum rows in a table.
pub const REC_MAX_ROWS: usize = 64;
/// Backing store (bytes) for interned `Str` cell values.
pub const REC_ARENA: usize = 4 * 1024;
/// Maximum column-name length (bytes).
pub const REC_COL_NAME: usize = 24;

/// Magic prefix of the binary wire encoding (`Table::encode`/`decode`). Lets a decoder reject a
/// non-record byte stream loudly instead of misparsing it.
const REC_WIRE_MAGIC: &[u8; 4] = b"GSR1";

/// A typed cell. `Str` points into the owning [`Table`]'s arena (no lifetimes, no heap).
#[derive(Clone, Copy)]
pub enum Value {
    /// A string, stored as an `(offset, length)` into the table's arena.
    Str { off: u32, len: u32 },
    /// An unsigned integer.
    Int(u64),
    /// An absent / null cell.
    Empty,
}

/// A numeric-column reducer (the `sum`/`min`/`max`/`avg` pipe aggregators, docs/scripting.md §5).
#[derive(Clone, Copy)]
pub enum AggOp { Sum, Min, Max, Avg }

/// Why an aggregate failed - each maps to a loud shell notice, never a silent 0.
#[derive(Clone, Copy)]
pub enum AggErr { NoColumn, NonNumeric }

/// A sink the renderers write bytes into - the caller's bridge to a console, a capture buffer,
/// or an IPC message. Implement it for whatever the bytes should go to.
pub trait RecordSink {
    /// Append `bytes` to the sink.
    fn put(&mut self, bytes: &[u8]);
}

/// A bounded table of typed rows - the canonical structured-pipe value. Owned column names (so a
/// `from_json` parse can name columns dynamically), rows of [`Value`], and a byte arena holding
/// the `Str` cells. All inline, no heap.
pub struct Table {
    col_names: [[u8; REC_COL_NAME]; REC_MAX_COLS],
    col_lens: [u8; REC_MAX_COLS],
    ncols: usize,
    rows: [[Value; REC_MAX_COLS]; REC_MAX_ROWS],
    nrows: usize,
    arena: [u8; REC_ARENA],
    alen: usize,
    overflow: bool,
}

impl Table {
    /// A new table with the given column names (in order).
    pub fn new(cols: &[&str]) -> Self {
        let mut t = Table {
            col_names: [[0u8; REC_COL_NAME]; REC_MAX_COLS], col_lens: [0; REC_MAX_COLS], ncols: 0,
            rows: [[Value::Empty; REC_MAX_COLS]; REC_MAX_ROWS], nrows: 0,
            arena: [0u8; REC_ARENA], alen: 0, overflow: false,
        };
        for c in cols { t.add_col(c.as_bytes()); }
        t
    }

    /// Add a column by name; returns its index (or `None` if full / name too long → overflow).
    pub fn add_col(&mut self, name: &[u8]) -> Option<usize> {
        if self.ncols >= REC_MAX_COLS || name.len() > REC_COL_NAME { self.overflow = true; return None; }
        let i = self.ncols;
        self.col_names[i][..name.len()].copy_from_slice(name);
        self.col_lens[i] = name.len() as u8;
        self.ncols += 1;
        Some(i)
    }

    /// Copy bytes into the arena and return a `Str` value (or `Empty` if the arena is full).
    pub fn intern(&mut self, s: &[u8]) -> Value {
        if self.alen + s.len() > REC_ARENA { self.overflow = true; return Value::Empty; }
        let off = self.alen as u32;
        self.arena[self.alen..self.alen + s.len()].copy_from_slice(s);
        self.alen += s.len();
        Value::Str { off, len: s.len() as u32 }
    }

    /// Append a row (values in column order). Loud-bounded: extra rows set [`Table::overflow`].
    pub fn add_row(&mut self, vals: &[Value]) {
        if self.nrows >= REC_MAX_ROWS { self.overflow = true; return; }
        for (i, v) in vals.iter().take(self.ncols).enumerate() { self.rows[self.nrows][i] = *v; }
        self.nrows += 1;
    }

    /// Number of columns.
    pub fn ncols(&self) -> usize { self.ncols }
    /// Number of rows.
    pub fn nrows(&self) -> usize { self.nrows }
    /// True if any bound was exceeded while building (rows/cols/arena/name).
    pub fn overflow(&self) -> bool { self.overflow }

    /// Column `c`'s name bytes.
    pub fn col_name(&self, c: usize) -> &[u8] { &self.col_names[c][..self.col_lens[c] as usize] }

    fn col_index(&self, name: &str) -> Option<usize> {
        (0..self.ncols).find(|&i| self.col_name(i) == name.as_bytes())
    }

    /// Resolve a cell's text: a `Str` from the arena, or the empty slice for non-strings.
    fn cell_str(&self, v: Value) -> &[u8] {
        match v {
            Value::Str { off, len } => &self.arena[off as usize..(off + len) as usize],
            _ => &[],
        }
    }

    // ── filters / ops ─────────────────────────────────────────────────────────────────────

    /// Keep only rows whose column `col` satisfies `<op> val` (in place). Ops: `=`/`==` `!=` `>`
    /// `<` `>=` `<=` `~`(contains). Numeric when both sides parse as numbers, else textual.
    /// Returns `false` (table unchanged) if `col` is not a column.
    pub fn filter(&mut self, col: &str, op: &str, val: &str) -> bool {
        let ci = match self.col_index(col) { Some(i) => i, None => return false };
        let mut keep = 0usize;
        for r in 0..self.nrows {
            if row_matches(self, r, ci, op, val) {
                if keep != r { self.rows[keep] = self.rows[r]; }
                keep += 1;
            }
        }
        self.nrows = keep;
        true
    }

    /// Keep only the named columns, in the given order (in place). Returns `false` (table
    /// unchanged) if any name is not a column. The arena (string storage) is untouched.
    pub fn select(&mut self, names: &[&str]) -> bool {
        let mut new_names = [[0u8; REC_COL_NAME]; REC_MAX_COLS];
        let mut new_lens = [0u8; REC_MAX_COLS];
        let mut map = [0usize; REC_MAX_COLS];
        let mut nc = 0usize;
        for &name in names {
            if name.is_empty() { continue; }
            match self.col_index(name) {
                Some(oi) if nc < REC_MAX_COLS => {
                    new_names[nc] = self.col_names[oi];
                    new_lens[nc] = self.col_lens[oi];
                    map[nc] = oi;
                    nc += 1;
                }
                Some(_) => {}
                None => return false,
            }
        }
        for r in 0..self.nrows {
            let old = self.rows[r];
            for i in 0..nc { self.rows[r][i] = old[map[i]]; }
            for i in nc..self.ncols { self.rows[r][i] = Value::Empty; }
        }
        self.col_names = new_names;
        self.col_lens = new_lens;
        self.ncols = nc;
        true
    }

    /// Order rows by column `col` (numeric when both ints, else by bytes), descending if
    /// `reverse`. Returns `false` (table unchanged) if `col` is not a column.
    pub fn sort(&mut self, col: &str, reverse: bool) -> bool {
        let ci = match self.col_index(col) { Some(i) => i, None => return false };
        let n = self.nrows;
        let arena = &self.arena; // disjoint field borrow: rows sorted mutably, arena read-only
        self.rows[..n].sort_unstable_by(|a, b| {
            let o = cmp_values(a[ci], b[ci], arena);
            if reverse { o.reverse() } else { o }
        });
        true
    }

    /// A cell as a number: an `Int` directly, or a `Str` of ASCII digits; `None` otherwise (a
    /// non-numeric or empty cell - the caller turns that into a loud [`AggErr::NonNumeric`]).
    fn cell_num(&self, v: Value) -> Option<u64> {
        match v {
            Value::Int(n) => Some(n),
            Value::Str { off, len } => {
                let b = &self.arena[off as usize..(off + len) as usize];
                if b.is_empty() { return None; }
                let mut acc: u64 = 0;
                for &c in b {
                    if !c.is_ascii_digit() { return None; }
                    acc = acc.saturating_mul(10).saturating_add((c - b'0') as u64);
                }
                Some(acc)
            }
            Value::Empty => None,
        }
    }

    /// Reduce a numeric column to a scalar (§5). Loud: `NoColumn` if `col` is not a column,
    /// `NonNumeric` if any cell is not a number - never a silent 0. Empty table reduces to 0.
    /// `avg` is integer (floor).
    pub fn aggregate(&self, col: &str, op: AggOp) -> Result<u64, AggErr> {
        let ci = self.col_index(col).ok_or(AggErr::NoColumn)?;
        if self.nrows == 0 { return Ok(0); }
        let (mut sum, mut mn, mut mx) = (0u64, u64::MAX, 0u64);
        for r in 0..self.nrows {
            let n = self.cell_num(self.rows[r][ci]).ok_or(AggErr::NonNumeric)?;
            sum = sum.saturating_add(n);
            if n < mn { mn = n; }
            if n > mx { mx = n; }
        }
        Ok(match op {
            AggOp::Sum => sum,
            AggOp::Min => mn,
            AggOp::Max => mx,
            AggOp::Avg => sum / self.nrows as u64,
        })
    }

    // ── renderers (edge formats) ──────────────────────────────────────────────────────────

    /// Render as an aligned text grid (the default view). String cells render in full (via the
    /// arena), so a long value is never silently clipped (§3.12).
    pub fn to_grid(&self, out: &mut impl RecordSink) {
        let mut w = [0usize; REC_MAX_COLS];
        for c in 0..self.ncols { w[c] = self.col_name(c).len(); }
        for r in 0..self.nrows {
            for c in 0..self.ncols {
                let n = cell_width(self, self.rows[r][c]);
                if n > w[c] { w[c] = n; }
            }
        }
        for c in 0..self.ncols {
            out.put(self.col_name(c));
            pad(out, w[c].saturating_sub(self.col_name(c).len()) + 2);
        }
        out.put(b"\n");
        let mut scratch = [0u8; 24];
        for r in 0..self.nrows {
            for c in 0..self.ncols {
                let n = match self.rows[r][c] {
                    Value::Str { .. } => { let s = self.cell_str(self.rows[r][c]); out.put(s); s.len() }
                    v => { let n = fmt_cell(self, v, &mut scratch); out.put(&scratch[..n]); n }
                };
                pad(out, w[c].saturating_sub(n) + 2);
            }
            out.put(b"\n");
        }
    }

    /// Render as a JSON array of objects (`to json`). Values are plain ASCII today - a real
    /// string-escaper is a documented follow-up.
    pub fn to_json(&self, out: &mut impl RecordSink) {
        out.put(b"[\n");
        for r in 0..self.nrows {
            out.put(b"  {");
            for c in 0..self.ncols {
                if c > 0 { out.put(b", "); }
                out.put(b"\"");
                out.put(self.col_name(c));
                out.put(b"\": ");
                match self.rows[r][c] {
                    Value::Int(_) => {
                        let mut b = [0u8; 24];
                        let n = fmt_cell(self, self.rows[r][c], &mut b);
                        out.put(&b[..n]);
                    }
                    Value::Empty => out.put(b"null"),
                    Value::Str { .. } => {
                        out.put(b"\"");
                        out.put(self.cell_str(self.rows[r][c]));
                        out.put(b"\"");
                    }
                }
            }
            out.put(if r + 1 < self.nrows { b"},\n" } else { b"}\n" });
        }
        out.put(b"]\n");
    }

    /// Render as YAML - a list of mappings (`to yaml`). String cells render in full.
    pub fn to_yaml(&self, out: &mut impl RecordSink) {
        let mut scratch = [0u8; 24];
        for r in 0..self.nrows {
            for c in 0..self.ncols {
                out.put(if c == 0 { b"- " } else { b"  " });
                out.put(self.col_name(c));
                out.put(b": ");
                match self.rows[r][c] {
                    Value::Str { .. } => out.put(self.cell_str(self.rows[r][c])),
                    v => { let n = fmt_cell(self, v, &mut scratch); out.put(&scratch[..n]); }
                }
                out.put(b"\n");
            }
        }
    }

    // ── from json (text → records bridge) ─────────────────────────────────────────────────

    /// Parse a JSON array of flat objects into a `Table` - the `from json` bridge. Bounded
    /// subset: `[ {"k": v, …}, … ]` with string / number / `true|false` / `null` values, **no
    /// nesting**. The first object defines the columns; later objects fill known columns (new
    /// keys ignored). On malformed input, returns `Err(reason)` with a bare static reason (the
    /// caller adds any `from json:` prefix).
    #[inline(never)]
    pub fn from_json(input: &[u8]) -> Result<Table, &'static str> {
        let mut t = Table::new(&[]);
        let b = input;
        let mut i = json_ws(b, 0);
        if i >= b.len() || b[i] != b'[' { return Err("expected a JSON array '[ … ]'"); }
        i = json_ws(b, i + 1);
        if i < b.len() && b[i] == b']' { return Ok(t); }
        let mut first_obj = true;
        loop {
            i = json_ws(b, i);
            if i >= b.len() || b[i] != b'{' { return Err("expected an object '{ … }'"); }
            i = json_ws(b, i + 1);
            let mut row = [Value::Empty; REC_MAX_COLS];
            if i < b.len() && b[i] == b'}' { i += 1; } else {
                loop {
                    i = json_ws(b, i);
                    let (ks, ke, kn) = match json_string(b, i) {
                        Some(x) => x,
                        None => return Err("expected a \"key\""),
                    };
                    i = json_ws(b, kn);
                    if i >= b.len() || b[i] != b':' { return Err("expected ':'"); }
                    i = json_ws(b, i + 1);
                    let v;
                    if i < b.len() && b[i] == b'"' {
                        let (vs, ve, vn) = match json_string(b, i) {
                            Some(x) => x, None => return Err("unterminated string"),
                        };
                        v = t.intern(&b[vs..ve]);
                        i = vn;
                    } else if b[i..].starts_with(b"true") { v = Value::Int(1); i += 4; }
                    else if b[i..].starts_with(b"false") { v = Value::Int(0); i += 5; }
                    else if b[i..].starts_with(b"null") { v = Value::Empty; i += 4; }
                    else if i < b.len() && (b[i] == b'-' || b[i].is_ascii_digit()) {
                        let s = i;
                        if b[i] == b'-' { i += 1; }
                        while i < b.len() && b[i].is_ascii_digit() { i += 1; }
                        if i < b.len() && (b[i] == b'.' || b[i] == b'e' || b[i] == b'E') {
                            while i < b.len() && !matches!(b[i], b',' | b'}' | b' ' | b'\t' | b'\n' | b'\r') { i += 1; }
                            v = t.intern(&b[s..i]);
                        } else {
                            v = core::str::from_utf8(&b[s..i]).ok().and_then(|x| x.parse::<u64>().ok())
                                .map(Value::Int).unwrap_or(Value::Empty);
                        }
                    } else {
                        return Err("unsupported value (nested objects/arrays not supported)");
                    }
                    let key = &b[ks..ke];
                    let ci = (0..t.ncols).find(|&c| t.col_name(c) == key);
                    let ci = match ci {
                        Some(c) => Some(c),
                        None if first_obj => t.add_col(key),
                        None => None,
                    };
                    if let Some(ci) = ci { row[ci] = v; }
                    i = json_ws(b, i);
                    if i < b.len() && b[i] == b',' { i += 1; continue; }
                    if i < b.len() && b[i] == b'}' { i += 1; break; }
                    return Err("expected ',' or '}'");
                }
            }
            t.add_row(&row);
            first_obj = false;
            i = json_ws(b, i);
            if i < b.len() && b[i] == b',' { i += 1; continue; }
            if i < b.len() && b[i] == b']' { return Ok(t); }
            return Err("expected ',' or ']'");
        }
    }

    // ── wire codec (records across a service boundary) ────────────────────────────────────

    /// Encode the table into a compact, *bounded* binary form for crossing a **service**
    /// boundary as records (no JSON round-trip). This is emphatically **not** JSON - it is the
    /// `Table` itself on the wire. Layout:
    ///
    /// ```text
    /// magic "GSR1" | ncols:u8 | nrows:u8
    /// per column:  name_len:u8 | name bytes
    /// per cell:    tag:u8 (0=empty 1=int 2=str)
    ///              int → val:u64-le ; str → len:u16-le | bytes ; empty → (nothing)
    /// ```
    ///
    /// Symmetric with [`Table::decode`]. The whole encoding is bounded by the table's own
    /// bounds; a producer sends it as one IPC message (≤ 4 KiB) for a small table, or chunks it
    /// - the shell drains chunks until EOT, then decodes. (A chunked producer must never emit a
    /// lone `0x04` chunk, which is the EOT marker; the magic guarantees the first chunk is not.)
    pub fn encode(&self, out: &mut impl RecordSink) {
        out.put(REC_WIRE_MAGIC);
        out.put(&[self.ncols as u8, self.nrows as u8]);
        for c in 0..self.ncols {
            let name = self.col_name(c);
            out.put(&[name.len() as u8]);
            out.put(name);
        }
        for r in 0..self.nrows {
            for c in 0..self.ncols {
                match self.rows[r][c] {
                    Value::Empty => out.put(&[0u8]),
                    Value::Int(i) => { out.put(&[1u8]); out.put(&i.to_le_bytes()); }
                    Value::Str { .. } => {
                        let s = self.cell_str(self.rows[r][c]);
                        out.put(&[2u8]);
                        out.put(&(s.len() as u16).to_le_bytes());
                        out.put(s);
                    }
                }
            }
        }
    }

    /// Decode the binary form produced by [`Table::encode`]. Validates the magic and every length
    /// against the table bounds and the buffer end - a truncated, oversized, or non-record buffer
    /// is a loud `Err`, never a misparse (§3.12). Strings intern into the new table's arena.
    #[inline(never)]
    pub fn decode(bytes: &[u8]) -> Result<Table, &'static str> {
        let b = bytes;
        if b.len() < 6 || &b[..4] != &REC_WIRE_MAGIC[..] { return Err("not a record stream (bad magic)"); }
        let mut p = 4usize;
        let ncols = b[p] as usize; p += 1;
        let nrows = b[p] as usize; p += 1;
        if ncols > REC_MAX_COLS { return Err("record has too many columns"); }
        if nrows > REC_MAX_ROWS { return Err("record has too many rows"); }
        let mut t = Table::new(&[]);
        for _ in 0..ncols {
            if p >= b.len() { return Err("truncated record (column count)"); }
            let nl = b[p] as usize; p += 1;
            if p + nl > b.len() { return Err("truncated record (column name)"); }
            if t.add_col(&b[p..p + nl]).is_none() { return Err("record column rejected (name too long)"); }
            p += nl;
        }
        for _ in 0..nrows {
            let mut row = [Value::Empty; REC_MAX_COLS];
            for cell in row.iter_mut().take(ncols) {
                if p >= b.len() { return Err("truncated record (cell tag)"); }
                let tag = b[p]; p += 1;
                *cell = match tag {
                    0 => Value::Empty,
                    1 => {
                        if p + 8 > b.len() { return Err("truncated record (int)"); }
                        let mut a = [0u8; 8];
                        a.copy_from_slice(&b[p..p + 8]); p += 8;
                        Value::Int(u64::from_le_bytes(a))
                    }
                    2 => {
                        if p + 2 > b.len() { return Err("truncated record (string length)"); }
                        let len = u16::from_le_bytes([b[p], b[p + 1]]) as usize; p += 2;
                        if p + len > b.len() { return Err("truncated record (string)"); }
                        let v = t.intern(&b[p..p + len]); p += len;
                        v
                    }
                    _ => return Err("bad record cell tag"),
                };
            }
            t.add_row(&row);
        }
        Ok(t)
    }
}

/// Parse a compact predicate token `col<op>val` (e.g. `mem>0`, `state=BlockRecv`, `name!=x`).
/// The operator is the longest match (`!=`/`>=`/`<=`/`==` before `=`/`>`/`<`/`~`); before it is
/// the column, after it the value. `None` if no operator is present.
pub fn parse_predicate(tok: &str) -> Option<(&str, &str, &str)> {
    for op in ["!=", ">=", "<=", "=="] {
        if let Some(i) = tok.find(op) { return Some((&tok[..i], op, &tok[i + op.len()..])); }
    }
    for op in ["=", ">", "<", "~"] {
        if let Some(i) = tok.find(op) { return Some((&tok[..i], &tok[i..i + 1], &tok[i + 1..])); }
    }
    None
}

// ── private helpers ───────────────────────────────────────────────────────────────────────

fn pad(out: &mut impl RecordSink, n: usize) {
    for _ in 0..n { out.put(b" "); }
}

/// Format one cell into `buf`, returning its length. Strings copy out (clamped to `buf`); ints
/// are decimal. Used for the numeric/scratch path; full-string rendering bypasses this.
fn fmt_cell(t: &Table, v: Value, buf: &mut [u8; 24]) -> usize {
    match v {
        Value::Str { .. } => {
            let s = t.cell_str(v);
            let n = s.len().min(buf.len());
            buf[..n].copy_from_slice(&s[..n]);
            n
        }
        Value::Int(i) => {
            let mut tmp = [0u8; 20];
            let mut p = tmp.len();
            let mut x = i;
            loop { p -= 1; tmp[p] = b'0' + (x % 10) as u8; x /= 10; if x == 0 { break; } }
            let n = tmp.len() - p;
            buf[..n].copy_from_slice(&tmp[p..]);
            n
        }
        Value::Empty => 0,
    }
}

/// Display width of a cell: a string's full arena length, else its formatted (numeric) length.
fn cell_width(t: &Table, v: Value) -> usize {
    match v {
        Value::Str { len, .. } => len as usize,
        Value::Int(_) => { let mut b = [0u8; 24]; fmt_cell(t, v, &mut b) }
        Value::Empty => 0,
    }
}

/// Does row `r`'s column `ci` satisfy `<op> val`? Numeric if both are numbers, else textual.
fn row_matches(t: &Table, r: usize, ci: usize, op: &str, val: &str) -> bool {
    let cell = t.rows[r][ci];
    let cell_num = match cell {
        Value::Int(i) => Some(i),
        Value::Str { .. } => core::str::from_utf8(t.cell_str(cell)).ok().and_then(|s| s.parse::<u64>().ok()),
        Value::Empty => None,
    };
    if let (Some(cn), Ok(vn)) = (cell_num, val.parse::<u64>()) {
        return match op {
            "=" | "==" => cn == vn,
            "!=" => cn != vn,
            ">" => cn > vn,
            "<" => cn < vn,
            ">=" => cn >= vn,
            "<=" => cn <= vn,
            _ => false,
        };
    }
    let cs = t.cell_str(cell);
    let vb = val.as_bytes();
    match op {
        "=" | "==" => cs == vb,
        "!=" => cs != vb,
        "~" => contains(cs, vb),
        _ => false,
    }
}

/// Resolve a value to its comparable bytes (arena slice for `Str`, empty otherwise).
fn val_str<'a>(v: Value, arena: &'a [u8]) -> &'a [u8] {
    match v { Value::Str { off, len } => &arena[off as usize..(off + len) as usize], _ => &[] }
}

/// Order two cells: numeric when both are ints, else by bytes.
fn cmp_values(a: Value, b: Value, arena: &[u8]) -> Ordering {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x.cmp(&y),
        _ => val_str(a, arena).cmp(val_str(b, arena)),
    }
}

/// Byte-substring test (private copy; the shell keeps its own for `find`/`match`).
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() { return true; }
    if needle.len() > haystack.len() { return false; }
    (0..=haystack.len() - needle.len()).any(|i| &haystack[i..i + needle.len()] == needle)
}

fn json_ws(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && (b[i] == b' ' || b[i] == b'\t' || b[i] == b'\n' || b[i] == b'\r') { i += 1; }
    i
}

/// At a `"`, scan to the closing quote (a `\`-escaped char is skipped). Returns (content start,
/// content end, index past the closing quote). Escapes are passed through literally.
fn json_string(b: &[u8], i: usize) -> Option<(usize, usize, usize)> {
    if i >= b.len() || b[i] != b'"' { return None; }
    let start = i + 1;
    let mut j = start;
    while j < b.len() {
        if b[j] == b'\\' { j += 2; continue; }
        if b[j] == b'"' { return Some((start, j, j + 1)); }
        j += 1;
    }
    None
}
