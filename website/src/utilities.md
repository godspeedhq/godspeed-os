# The utilities

The commands you type. Every one of them is documented here, and the documentation is the source of
truth the implementation is held to - `utilities/0_conventions.md` is the shared contract all of them
obey, and a utility that breaks it is a bug in the utility.

## Why they look the way they do

A GodspeedOS utility is not a Unix utility with the corners knocked off. The primitives Unix builds on
do not exist here: no `fork`, no `exec`, no inherited file descriptors, no ambient `stdin`, no
environment inheritance, no signals. What replaces them is capabilities.

```
   UNIX                              GODSPEEDOS
   ls /data | grep .txt              ls /data | match .txt
   ────────────────────              ──────────────────────
   fork + exec + pipe(2)             the shell creates an ENDPOINT
   fd 1 inherited by the child       and grants one end to each side
   ambient access to /data           `ls` holds a cap to `fs`, or it
                                     cannot read anything at all
```

Three consequences you will feel immediately:

- **`rm -rf /` cannot exist.** A utility can only touch what it was granted. There is no ambient
  authority to leak through a fork.
- **Words, not flags.** `write append /f.txt text`, not `write -a`. A flag is a character to memorise;
  a word is readable a year later. (`0_conventions.md` §1.)
- **Every utility answers `version` and `help`.** Not as a courtesy - as a contract, checked.

## The full set

Each links to its full specification: what it is, what it may do, what it may *not* do, and how it
fails.

### Files and directories

| | |
|---|---|
| [`ls`](utilities/ls.md) | list a directory |
| [`cd`](utilities/cd.md) | change current location |
| [`read`](utilities/read.md) | print a file's contents |
| [`write`](utilities/write.md) | create, overwrite, append, or prepend a file |
| [`mkdir`](utilities/mkdir.md) | create a directory |
| [`copy`](utilities/copy.md) | copy a file or a whole subtree |
| [`move`](utilities/move.md) | relocate a file |
| [`rename`](utilities/rename.md) | rename a file or directory in place |
| [`delete`](utilities/delete.md) | remove a file, directory, or whole subtree |
| [`find`](utilities/find.md) | search the tree for a name |
| [`tree`](utilities/tree.md) | print the directory hierarchy |
| [`edit`](utilities/edit.md) | the full-screen editor (a bounded piece table - any file size) |
| [`fcap`](utilities/fcap.md) | open a file as a real kernel capability |
| [`drives`](utilities/drives.md) | manage attached disks |

### Text and records

| | |
|---|---|
| [`echo`](utilities/echo.md) | write text |
| [`match`](utilities/match.md) | keep the lines that match (the grep-equivalent) |
| [`count`](utilities/count.md) | how many lines, words, and bytes |
| [`sort`](utilities/sort.md) | order the lines |
| [`first` / `last`](utilities/first-last.md) | keep the first or last N lines |
| [`where` / `select` / `to` / `from`](utilities/records.md) | typed record pipelines |

### The system

| | |
|---|---|
| [`status`](utilities/status.md) | what is running, and where |
| [`observe`](utilities/observe.md) | the live full-screen view |
| [`caps`](utilities/caps.md) | what authority a service actually holds |
| [`whatis`](utilities/whatis.md) | what a name is: built-in, script, pipe stage, or service |
| [`events`](utilities/events.md) | logs, IPC traces, metrics, and capturing them to disk |
| [`mem`](utilities/mem.md) | memory |
| [`cores`](utilities/cores.md) | the cores that came up |
| [`uptime`](utilities/uptime.md) | how long since boot |
| [`date`](utilities/date.md) | the wall clock |
| [`about`](utilities/about.md) | what this system is |
| [`version`](utilities/version.md) | version, of anything |
| [`clear`](utilities/clear.md) | clear the screen |

### Lifecycle

| | |
|---|---|
| [`spawn`](utilities/spawn.md) | start a service |
| [`kill`](utilities/kill.md) | stop one |
| [`restart`](utilities/restart.md) | stop and start, possibly on another core |
| [`chaos`](utilities/chaos.md) | kill things deliberately, and see what survives |
| [`reboot`](utilities/reboot.md) | restart the machine |
| [`poweroff`](utilities/poweroff.md) | **not provided** - and the page says why |

### Networking

| | |
|---|---|
| [`net`](utilities/net.md) | am I on the network? |
| [`sock`](utilities/sock.md) | a UDP socket as a capability |
| [`ping`](utilities/ping.md) | continuous ICMP echo |

### Scripting

| | |
|---|---|
| [`run`](utilities/run.md) | execute a script of commands |
| [`assert`](utilities/assert.md) | verify a result or output (the test verb) |
| [`result`](utilities/result.md) | the previous command's result (Ok / Err) |
| [`wait`](utilities/wait.md) | wait |
| [`fmt`](utilities/fmt.md) | format a script to the GodspeedOS standard |

### The shared contract

| | |
|---|---|
| [Conventions](utilities/conventions.md) | the thirteen rules every utility above obeys |

## One page worth reading even if you skip the rest

[`poweroff`](utilities/poweroff.md) documents a utility that **does not exist**. It was built, tested,
and removed - the firmware's ACPI `\_PTS` method could not be executed without an AML interpreter, and
adding one to reach a power state was not a trade worth making. The page records what was tried, what
stopped it, and what would have to change.

That is the house style: a limitation that cannot be closed is written down, never quietly left
(§26.7).
