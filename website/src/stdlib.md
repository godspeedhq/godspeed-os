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

```rust
use godspeed as gs;

let mut fs = gs::fs::Fs::new(ctx);

// There is no `read_to_string`. GodspeedOS has no heap (CLAUDE.md 26.6.1), so the CALLER owns the
// buffer and the bound is visible in the source.
let mut buf = [0u8; 4096];
let n = fs.read_into("/data/message.txt", &mut buf)?;
gs::io::println(ctx, core::str::from_utf8(&buf[..n]).unwrap_or("<not utf-8>"));
```

`examples/stdlib-hello` is the same program as a complete, buildable service.

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
