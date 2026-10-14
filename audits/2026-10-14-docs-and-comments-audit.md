# Docs and comments audit, feat/filesystem-layout (2026-10-14)

**Scope.** What this branch wrote or made stale: the filesystem layout (phase 1), the `[q]` quit rule and its
slices (`backlog/89`), and the typed jobs that `[b]` detaches. Three read-only passes, each checked against the
code in the working tree: the design documents and indexes (`docs/`, `CLAUDE.md`, `services/CLAUDE.md`,
`osdev/CLAUDE.md`, `website/src/`), the utility specs (`utilities/`) with `backlog/89`, and the code comments the
branch added or left beside changed code. Every finding below was re-read against the code before it was
changed. Dated evidence (`audits/`, `backlog/fixed/`, dated log sections) is left as written.

**Result.** 40 defects: 37 in documents and comments, corrected in the same change, and 3 in code, recorded in
`backlog/89` and fixed with it (this branch's own work, so no new backlog entry).

## Documents (corrected)

| Where | Was | Now |
|---|---|---|
| `docs/filesystem-layout.md` 1 | "What is true today" - everything in the root | "Before phase 1", pointing at section 13 for where each file is now |
| `docs/filesystem-layout.md` 3 | first use "makes disks formatted before the change work without a reformat" | an old disk needs `drives flash`; first use only brings back a folder removed by hand (the file's own line 8 said so) |
| `docs/filesystem-layout.md` 3 | `fs` learns "two layout names" (then named six) | the standard tree: `system` with four folders, and `users/default` |
| `docs/filesystem-layout.md` 6 | "today it writes `/notes.txt`", "a bare `cd` ... will go to the session's home" | both built; said in the past for before phase 1 |
| `docs/filesystem-layout.md` 13 | "the five folders" | five paths in `LAYOUT`, seven folders with `/system` and `/users` |
| `docs/CLAUDE.md` | "the folder in the prompt"; `/audio.settings`; `/wifi.keys` | plain `gsh> ` with `pwd` and `home`; `/system/services/<driver>/settings`; `/system/shared/wifi/keys` |
| `utilities/35_fcap.md` | `/.fcap-selftest`, `/.fcap_append`, `/.fcap-gs` | in `/system/scratch/shell/` |
| `docs/stdlib-design.md` | `if dir /churn`, "it lists `/churn`" | `/system/scratch/churn` |
| `docs/wifi.md` (2), `docs/wifi-usb.md` | `/.gsh_history`, `/persist.conf` | the history file `/users/default/history`; `/system/services/events/sticky` |
| `backlog/32` | "creates `/clock.last` and `.gsh_history`" | the new paths, and that the expected count was not re-measured |
| `osdev/CLAUDE.md` | `files` 252 checks, `file-cap` 15 | 265, 16 (the counts in `docs/filesystem-layout.md` 13) |
| `utilities/17_cd`, `18_read`, `25_find`, `26_tree`, `27_match`, `28_count`, `29_sort` | "Does NOT currently conform to rule 10 ... lent no notice" | conforms since 2026-10-13: every `fs` handle the shell makes is lent the command's watch; which are measured by `osdev test quit` |
| `utilities/16_dir.md` | the piped form "lends no notice ... cannot be ended with `q`" | the same watched handle |
| `utilities/41_sock.md` | "NOT currently `q`-escapable ... `Net::new`" | `Net::with_notice`, and that its notice is the request's clock, not the command's |
| `utilities/56_wifi.md` (2 rows) | `radio on`/`off`/`off hard` "BLOCK ... no `[q]` ... a kernel `Call`" | `[q] quit` past 2 s; `q` stops the wait, not the request, and says so |
| `utilities/0_conventions.md` rule 9 | listing the root "offered `/users/default/history`" | it offered `/.gsh_history`, where the history then lived |
| `utilities/0_conventions.md` rule 10 | the radio power "Recorded exception", undated; "Still silent" list; "not yet covered: the fs handles"; `gs::net` handles "lent `.watching`" | marked superseded with the date; the `quit-ok` exceptions and `scripts/quit_wait_check.py` named; `gs::net` as it is |
| `utilities/55_background.md`, `backlog/89` | a typed job's output "is the same either way" | the same lines after one naming the job; a typed job is numbered when detached or left running by a `q` |
| `utilities/51_churn.md` | the TORN line quoted short | the whole line |
| `backlog/89` | "Nothing is fixed yet", "The operator chooses" | the route built and measured; what is open |
| `services/copier/CLAUDE.md` | `render_verdict` (removed), `jobs` 36/0 | the check and scrub branch writing the shell's lines; 60/0 |

## Code comments (corrected)

| Where | Defect |
|---|---|
| shell `sh_write_within`, `fs_no_answer`, `cmd_selfcheck_parts`; fs `mkdir_parents`, `serve_once` | a doc comment left on the item inserted under it, so two blocks described one item and the other had none |
| shell `ask_with_quit_notice` | its doc comment and `#[inline(always)]` sat on `ask_watched`, inserted between them |
| shell `quit` field | "the last `wifi_ask` or `audio_ask` ... set false at the start of every ask": it is the command's quit, cleared by `begin_command` |
| shell `drain_service_with` | "the other drains take a `&ServiceContext`": they take `&ShellCtx` |
| shell `Cwd` | "both default to root": both start at `HOME` |
| shell `fs_waiting`, `ask_watched`, `JobStart`, `wifi_radio`, `wifi_radio_hard_off` | the watch's part left out; "every service wait is built on this"; "as job `.0`"; "a blocking Call cannot poll the console" |
| `stdlib` `Fs::watch`, `Fs::watching`, `Net::watching`, `request_watched_sifted` | "the notice and keep are not used" / "the plain wait" / "takes only the reply `mine` accepts" - true only when the watch is active |
| osdev `gsfs_layout`; shell_test (2) | "only `mkfs` and `script-disk` call it" (the script suite does); "a script that cannot be stopped"; `.gsh_history` |

## Code (fixed, recorded in `backlog/89`)

- A typed `copy` or `delete` offered `[b]` during its first look at the path even with a job running, where the
  command then runs in the shell and `b` does nothing.
- `copier`'s `fs_mkdir` had no caller since churn moved to its scratch folder.
- `scripts/quit_wait_check.py` counted prose that mentions its marker as a marker; a marker now starts its line,
  and the selftest has a probe for it.

## Not changed, and why

- `audits/2026-10-13-quit-audit.md`: dated evidence, framed as of `a94c277d`; nothing in it reads as current.
- `backlog/71`'s "`/wifi.keys`" in a dated proposal's surface list: left as written; it is a proposal of its date.
- `q` on `wifi radio off hard` stopping the wait and not the cut: the operator's question (rule 11), recorded in
  `backlog/89`.
