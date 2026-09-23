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

use godspeed::{fs, io, Error, ServiceContext};

// `#[no_mangle]` is itself covered by the `unsafe_code` lint (an exported symbol can collide), and
// the entry symbol must be exported because the linker looks for `service_main` by name. So this
// ONE `#[allow]` is expected on this ONE item. Anywhere else it is rejected.
#[allow(unsafe_code)]
#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    let mut fs = fs::Fs::new(&ctx);

    // There is no `read_to_string`. GodspeedOS has no heap (CLAUDE.md 26.6.1), so the CALLER owns
    // the buffer and the bound is visible in the source.
    let mut buf = [0u8; 4096];
    match fs.read_into("/data/message.txt", &mut buf) {
        Ok(n) => io::println(&ctx, core::str::from_utf8(&buf[..n]).unwrap_or("<not utf-8>")),
        Err(e) => io::report(&ctx, "read", e),
    }

    // A program does not return. There is no `exit`: every runnable thing here is a service, and a
    // service that has finished its work waits to be stopped.
    loop { ctx.yield_cpu(); }
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
log_write    = true       # write the kernel log ring and serial (`ctx.log`), which is NOT the screen
```

Those last two are different capabilities and it is worth knowing which is which the first time
rather than the second: `log_write` is the kernel log, `console_push` is the display. A program that
declares only `log_write` and calls `io::println` compiles, passes `osdev validate`, passes every
checker, and prints nothing at all.

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
