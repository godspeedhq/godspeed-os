//! `roster` — an example **record-producing** pipe service (Appendix D, `docs/records.md`).
//!
//! Where `greet` emits text lines, `roster` emits **structured records**: it builds a typed
//! `Table` with the SDK (`godspeed_sdk::record`), serializes it with the **binary wire codec**
//! (`Table::encode`), and sends the bytes through the delegated pipe cap. The shell decodes the
//! stream straight back into a `Table` (it knows `roster` is a record service), so the record
//! verbs operate on a real field with **no JSON round-trip**:
//!
//! ```text
//! gsh> roster | where role=core
//! gsh> roster | sort reverse seat | select name seat
//! gsh> roster | to json            (records → JSON only at the edge, if you want it)
//! ```
//!
//! Like `greet`, `roster` declares **no** send peers — its only way out is the SEND cap the shell
//! delegates at spawn (`send_peers[0]`). Authority is granted at composition time, not held.

#![no_std]
#![no_main]

use godspeed_sdk::{Message, RecordSink, ServiceContext, Table, Value};

/// A `RecordSink` that accumulates the encoded bytes into a fixed buffer (no heap). The wire
/// encoding for a handful of rows is well under one IPC message (4 KiB); overflow is flagged,
/// never silent.
struct BufSink {
    buf: [u8; 1024],
    len: usize,
    overflow: bool,
}
impl RecordSink for BufSink {
    fn put(&mut self, b: &[u8]) {
        let end = (self.len + b.len()).min(self.buf.len());
        let n = end - self.len;
        self.buf[self.len..end].copy_from_slice(&b[..n]);
        if n < b.len() {
            self.overflow = true;
        }
        self.len = end;
    }
}

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    ctx.log("roster: ready");

    // Build a small typed table — data this service "owns". Columns: name / role / seat.
    // `seat` is an illustrative Int column (a desk number, NOT a CPU core) — roster is a fixed
    // demo dataset, the same 4 rows on any machine; real core counts live in `cores`/`status`.
    let mut t = Table::new(&["name", "role", "seat"]);
    let rows: [(&[u8], &[u8], u64); 4] = [
        (b"Matthew", b"core", 1),
        (b"Mark", b"worker", 2),
        (b"Luke", b"courier", 3),
        (b"John", b"worker", 4),
    ];
    for (name, role, seat) in rows.iter() {
        let n = t.intern(name);
        let r = t.intern(role);
        t.add_row(&[n, r, Value::Int(*seat)]);
    }

    // Serialize with the binary wire codec — the `Table` itself on the wire, not JSON. The shell
    // decodes it straight back into records (no `from json`).
    let mut sink = BufSink { buf: [0u8; 1024], len: 0, overflow: false };
    t.encode(&mut sink);

    // send_peers[0] is the SEND cap the shell delegated to the pipe sink.
    match ctx.send_peer_at(0) {
        Some(dst) => {
            // The encoding fits in one 4 KiB message; then the EOT end-of-stream marker. (A larger
            // table would be chunked — never as a lone 0x04, which is EOT.)
            let _ = ctx.send_by_handle(dst, &Message::from_bytes(&sink.buf[..sink.len]));
            let _ = ctx.send_by_handle(dst, &Message::from_bytes(&[0x04]));
            ctx.log("roster: sent a 4-row table via the binary record codec through the pipe cap");
        }
        None => {
            ctx.log("roster: no pipe cap was delegated — nothing to send to");
        }
    }

    loop {
        ctx.yield_cpu();
    }
}
