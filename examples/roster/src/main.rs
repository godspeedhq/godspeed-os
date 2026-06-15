//! `roster` — an example **record-producing** pipe service (Appendix D, `docs/records.md`).
//!
//! Where `greet` emits text lines, `roster` emits **structured records**: it builds a typed
//! `Table` with the SDK (`godspeed_sdk::record`), renders it to JSON, and sends the bytes through
//! the delegated pipe cap — exactly the path any third-party service uses to feed a record pipe
//! *without* a new kernel surface. The shell's `| from json` lifts the JSON back into records, so
//! `roster | from json | where role=core` filters on a real field:
//!
//! ```text
//! gs> roster | from json | where role=core
//! gs> roster | from json | sort reverse core | select name core
//! ```
//!
//! Like `greet`, `roster` declares **no** send peers — its only way out is the SEND cap the shell
//! delegates at spawn (`send_peers[0]`). Authority is granted at composition time, not held.

#![no_std]
#![no_main]

use godspeed_sdk::{Message, RecordSink, ServiceContext, Table, Value};

/// A `RecordSink` that accumulates the rendered bytes into a fixed buffer (no heap). The JSON for
/// a handful of rows is well under one IPC message (4 KiB); overflow is flagged, never silent.
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

    // Build a small typed table — data this service "owns". Columns: name / role / core.
    let mut t = Table::new(&["name", "role", "core"]);
    let rows: [(&[u8], &[u8], u64); 3] = [
        (b"atlas", b"worker", 1),
        (b"hermes", b"courier", 2),
        (b"vesta", b"core", 0),
    ];
    for (name, role, core) in rows.iter() {
        let n = t.intern(name);
        let r = t.intern(role);
        t.add_row(&[n, r, Value::Int(*core)]);
    }

    // Render to JSON — the wire-friendly edge format the shell's `from json` lifts back to
    // records. (Crossing the boundary *as records* is the future bounded wire codec.)
    let mut sink = BufSink { buf: [0u8; 1024], len: 0, overflow: false };
    t.to_json(&mut sink);

    // send_peers[0] is the SEND cap the shell delegated to the pipe sink.
    match ctx.send_peer_at(0) {
        Some(dst) => {
            // The JSON fits in one 4 KiB message; then the EOT end-of-stream marker.
            let _ = ctx.send_by_handle(dst, &Message::from_bytes(&sink.buf[..sink.len]));
            let _ = ctx.send_by_handle(dst, &Message::from_bytes(&[0x04]));
            ctx.log("roster: sent a 3-row table as JSON through the delegated pipe cap");
        }
        None => {
            ctx.log("roster: no pipe cap was delegated — nothing to send to");
        }
    }

    loop {
        ctx.yield_cpu();
    }
}
