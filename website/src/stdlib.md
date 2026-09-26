# Writing a program for GodspeedOS

This is the page for someone who wants to **use** GodspeedOS rather than understand it. You should
not need MISCIS, the capability table, or the IPC wire format to read a file.

## The API reference

**[The standard library API reference](api/godspeed/index.html)** - `godspeed`, imported as `gs`.
Every module carries its own documentation, including what each call blocks on, what authority it
needs, and which failures are safe to retry.

**[The SDK API reference](api/godspeed_sdk/index.html)** - `godspeed_sdk`, the layer underneath.
Reach for this when you are writing a driver or a service that needs the raw syscall seam; an
ordinary program should not have to.

## Start here

A whole program - every line of it, because the parts around your code are not guessable:

```rust
#![no_std]                  // no operating system underneath this one
#![no_main]                 // the entry point is `service_main`, not `main`
#![deny(unsafe_code)]       // required of every program; see the note below

use godspeed::{self as gs, Error, ServiceContext};

// `#[no_mangle]` is itself covered by the `unsafe_code` lint (an exported symbol can collide), and
// the entry symbol must be exported because the linker looks for `service_main` by name. So this
// ONE `#[allow]` is expected on this ONE item. Anywhere else it is rejected.
#[allow(unsafe_code)]
#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    let mut fs = gs::fs::Fs::new(&ctx);

    // There is no `read_to_string`. GodspeedOS has no heap (CLAUDE.md 26.6.1), so the CALLER owns
    // the buffer and the bound is visible in the source.
    let mut buf = [0u8; 4096];
    match fs.read_into("/data/message.txt", &mut buf) {
        Ok(n) => gs::io::println(&ctx, core::str::from_utf8(&buf[..n]).unwrap_or("<not utf-8>")),
        Err(e) => gs::io::report(&ctx, "read", e),
    }

    // A program does not return. There is no `exit`: every runnable thing here is a service, and a
    // service that has finished its work waits to be stopped.
    loop { gs::task::yield_now(&ctx); }
}
```

### And a contract, or it reaches nothing

Authority is granted, never assumed. A program with no contract entry for `fs` gets
`Error::Unreachable` from its first call - not a crash, and not a silent nothing. Put this beside
your `Cargo.toml`, in `contracts/<name>.toml`:

```toml
name    = "hello"
version = "0.1.0"

[resources.memory]
request = "8MiB"
limit   = "16MiB"

[capabilities]
ipc_send     = ["fs"]     # talk to the filesystem. Drop this and `read_into` returns Unreachable
ipc_receive  = ["hello"]  # your own endpoint, named after you
console_push = true       # PUT TEXT ON THE SCREEN. Drop this and `io::println` runs and nothing
                          # appears - no error, no warning, just a silent program
```

Note what is **not** there: `log_write`. That grants `ctx.log()`, which writes the kernel log ring and
serial - a different destination from the screen - and the program above never calls it. Everything
in `gs::io`, `io::report` included, goes to the screen and needs `console_push`. Add `log_write` when
you actually call `ctx.log`, and not before.

A program that declares only `log_write` and calls `io::println` compiles, passes `osdev validate`,
passes every checker, and prints nothing at all - its error messages included, because `io::report`
goes to the screen too.

**If your output is missing, read your contract before you read your code.**

Ask for what you use and nothing more: the contract is the reviewable statement of what your program
may do (CLAUDE.md 26.9).

`examples/stdlib-hello` is this same program, complete and buildable.

## The one thing to get right

Every call returns `Result<_, gs::Error>`, and the error that matters most is the one describing an
operation whose outcome **nobody knows**:

```rust
match fs.write("/data/log.txt", b"hello") {
    Ok(()) => {}
    Err(e) if e.retry_is_safe() => { /* nothing happened: try again */ }
    Err(e) => { /* it MAY have happened: do NOT blindly retry */ }
}
```

`Error::OutcomeUnknown` means the request left and no answer came back. The write may have
committed. Retrying it is not a retry, it is a **second write**, and for anything that is not
idempotent that is a different bug from the one you were recovering from.

`retry_is_safe()` returns `false` for it, so that nobody has to derive the rule themselves. This is
the single failure the [Stranger Test](constitution.md) watches hardest, because it is invisible in
a passing build.

## If your program SERVES other tasks

Most programs do not, and can skip this. A service does: it answers requests on its own endpoint, and
that is the same endpoint replies from `fs` and `net-stack` arrive on. So there is a question with a
wrong answer that looks like the right one - **what happens to a client request that lands while you
are waiting for a reply?**

The library's answer is that it does not touch it. A call waits on the reply capability it sent, so
the kernel hands back that reply and leaves everything else queued for your own loop. You do not have
to drain anything, and nothing you did not ask for is consumed.

```rust
// A service loop. `fs.read_into` may block for seconds; a client that speaks during it is still
// waiting on your endpoint afterwards, not lost.
loop {
    let req = ctx.recv();
    let mut buf = [0u8; 4096];
    let n = fs.read_into("/data/answer.txt", &mut buf)?;
    reply(&req, &buf[..n]);
}
```

Two exceptions, both of which say so where you reach for them:

- **A capability you hold** - an open file (`gs::file::File`), a socket or a connection
  (`gs::net`) - is invoked rather than sent by name, and the kernel routes its reply the same way.
  Anything else that arrives during one of those is **held** for you: drain `take_held()` in a loop
  after each operation and feed what comes back into your own loop. They are real client requests,
  and only you can answer them.
- **A handle built with `Fs::with_notice`** polls for an operator pressing `q`, which means it takes
  whatever arrives. That is right for a shell, which serves nobody, and wrong for a service.

## What the library will not do

- **Allocate on your behalf.** No heap, by design. You pass the buffer.
- **Hide a failure.** There is no silent retry and no fallback path. If something did not happen,
  the call says so, and if it might have happened, it says that instead.
- **Grant authority.** A handle is not a permission. `gs::fs::Fs::new` carries nothing of its own:
  every call rides the capability your contract was already granted, and a service that never asked
  for the filesystem gets `Error::Unreachable` from the first call. The library makes granted
  authority convenient to use; it never makes ungranted authority available.

## The design report

[How the library was designed, and what building it changed](design/stdlib.md) - including the
mistakes found by migrating real services onto it, the friction reported rather than worked around,
and what is deliberately not built yet.

[The brief it was built from](design/stdlib-brief.md) - the original requirement, annotated with
what was met and what was not.
