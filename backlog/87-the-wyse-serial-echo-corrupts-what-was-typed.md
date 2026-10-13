# 87. The Wyse's echo of a typed line was corrupted while the shell received it clean

**Status: OPEN, 2026-10-13, last seen 2026-09-16.** Moved out of `backlog/fixed/29` when that entry
was closed: it was in that entry's "Also still open" list and recorded nowhere else. Whether it still
happens is not known; nothing has looked since.

## Evidence

Dell Wyse 5070, 2026-09-16 (`backlog/fixed/29`, "And the console echo is NOT the input"). A typed
`tcp 192.168.4.40 7777 hello` echoed as

```
tcp 192.168,4.40 7777 hello4.40 7777 hello.4.40 7777 hello
```

with a comma in the address and the tail repeated three times, while `net-stack` received a clean
`192.168.4.40` and a 5-byte `hello`. Seen alongside `xhci: probes 0/2172 ok` (every hub probe
failing) and `a HID report arrived with no interrupt - polling input at the 10ms tick`.

## What is ruled out

- Not the shell's input buffer: what it parsed was right.

## Next step

On the next Wyse boot, type a long line with punctuation at the prompt and compare the echo on the
screen and on serial with what the shell ran. Clean on both: close this as not reproduced since
2026-09-16. Corrupted on one only: the fault is in that output path (`console` or the kernel serial
writer; `backlog/04`'s splice is the one known way serial mixes lines).
