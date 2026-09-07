<!-- SPDX-License-Identifier: GPL-2.0-only -->
# Audit Report - 2026-07-15

```text
  ┌────────────────────────────────────────────────────────────────────┐
  │  GodspeedOS  ·  Full-Trilogy Commandment Audit                     │
  │  branch feat/aarch64-prep @ a9c8566                                │
  │  13 auditors  ·  kernel + userspace + documentation               │
  └────────────────────────────────────────────────────────────────────┘
```

> **This is a dashboard, not a decision.** It reconciles from the three living audit docs and *displays*
> what they record. Those docs are the source of truth; this report only reflects it. It states what was
> found and each finding's current status - it does **not** prescribe what happens next (that belongs to
> a ticketing system, not here). Green tick = audited and clean of MED-or-above; ⚠ = carries a MED+
> finding; a status label is a fact about repo state, never an instruction.

Legend: ✅ clean (of MED+) · ⚠ MED+ finding present · ◻ low-severity note only

---

## 1. Trilogy at a glance

```text
   AUDIT              SCOPE            HIGH   MED   LOW    VERDICT
   ─────────────────────────────────────────────────────────────
   Kernel             5 subsystems      0     1     4      ⚠
   Userspace          5 groups          0     5     3      ⚠     (+1 carried MED)
   Documentation      3 probes          0     0     1      ✅
   ─────────────────────────────────────────────────────────────
   TOTAL                                0     6     8            (+1 carried)
```

| Audit | System of record | Result |
|-------|------------------|--------|
| **Kernel** | `docs/kernel-audit.md` - Audit 4 | ⚠ one MED-HIGH, four LOW |
| **Userspace** | `docs/userspace-audit.md` - Audit 3 | ⚠ five MED, three LOW, one carried |
| **Documentation** | `docs/documentation-audit.md` - Audit 2 | ✅ zero MED+, one LOW |

**Top-line fact:** 0 HIGH. No panic, wedge, corruption, or authority-escape observed on any path.

---

## 2. Findings by severity

```text
   HIGH   ·                                             0
   MED    ██████████████████████████████████           6      (+ ▒ carried = 1)
   LOW    ████████████████████████████████████████████ 8
          └────┴────┴────┴────┴────┴────┴────┴────┘
          0    1    2    3    4    5    6    7    8   findings   (each █ = 1)
```

| Severity | Count | IDs |
|----------|:-----:|-----|
| **HIGH** | 0 | - |
| **MED** | 6 | `T1` `F1` `N1` `S1` `XH-1` `XH-2` |
| **MED (carried)** | 1 | `M4` |
| **LOW** | 8 | `M1` `M2` `K-a` `K-b` `XH-3` `XH-4` `XH-5` `DA1` |

## 3. Findings by status

```text
   Fixed (this run)        ██████                       3
   Open / staged           ██████████████████████       11
   Deferred                ██                            1
                           └──────┴──────┴──────┴──────┘
                           0      4       8      12   findings
```

| Status | Count | Meaning (state, not instruction) |
|--------|:-----:|----------------------------------|
| **Fixed** | 3 | landed this run (`DA1`; `M1`/`M2` doc-drift banners) |
| **Open / staged** | 11 | recorded in the living doc; not yet actioned |
| **Deferred** | 1 | on record, intentionally not scheduled (`M4`) |

---

## 4. Coverage grids

### 4.1 Kernel - `kernel-audit.md` Audit 4

```text
   syscall+ipc+interrupt   [ ✅ ]
   capability              [ ◻ ]   2 LOW
   task+scheduler          [ ⚠ ]   T1
   memory+smp              [ ◻ ]   2 LOW
   arch + boundary seam    [ ✅ ]   0 boundary leaks (checked 4 ways)
```

### 4.2 Userspace - `userspace-audit.md` Audit 3

```text
   block-driver + fs                    [ ⚠ ]   F1
   nic-driver + net-stack               [ ⚠ ]   N1  (+ M4 carried)
   supervisor + logger                  [ ⚠ ]   S1
   xhci + ehci                          [ ⚠ ]   XH-1 XH-2 (+ 3 LOW)
   shell + chaos + observe + probe      [ ✅ ]
```

### 4.3 Documentation - `documentation-audit.md` Audit 2

```text
   grokability panel (cold)     [ ✅ ]   median 7/10
   Audit-1 fixes legible?       [ ✅ ]   confirmed by 2 cold reads
   seeded review (field guide)  [ ✅ ]   cluster caught + anti-patterns.md cited
```

---

## 5. Findings ledger

Read-only. Full detail (evidence, line numbers, and any fix) lives in the cited living doc.

| ID | Sev | Audit | Location | What | Status |
|----|-----|-------|----------|------|--------|
| **T1** | MED-HIGH | Kernel | `task/mod.rs` | Page-table + ELF + stack frames leak on a spawn failure after `loader::load()`; ratchets under respawn retries. §26.6 / IX (not a wedge). | Open |
| **F1** | MED | Userspace | `fs` | `OP_RESET`/`OP_FLASH` drop the volume without revoking open file caps -> delegated-slot leak. | Open |
| **N1** | MED | Userspace | `net-stack` | Sockets never closed/revoked -> 8-slot table leaks; 9th open fails with a misleading "no NIC" error. | Open |
| **S1** | MED | Userspace | `supervisor` | Logger's fresh-boot spawn bypasses the name-map, silently disabling its dropped-notification recovery backstop. | Open |
| **XH-1** | MED | Userspace | `xhci` | HC-wedge detection not ported to the hub/downstream path; a downstream wedge can re-freeze all cores. | Open |
| **XH-2** | MED | Userspace | `xhci` | Hub-walk Address-Device failure leaks a controller slot (no `disable_slot`). | Open |
| **M4** | MED | Userspace | `net-stack` | `!have_mac` gate never re-reconciles a cached IP on a different-subnet re-DHCP. | Deferred |
| **M1** | LOW | Kernel | `memory/` | `TaskMemoryOwner`/`ownership.rs`/`page.rs` dead; `CLAUDE.md` described them as live (III drift). | Fixed (doc); code Open |
| **M2** | LOW | Kernel | `smp/placement.rs` | Dead `static mut` placement stub; `task/CLAUDE.md` cited it as live. | Fixed (doc); code Open |
| **K-a** | LOW | Kernel | `capability/cap.rs` | `Capability::validate`/`narrow_for_grant` dead (re-implemented inline). | Open |
| **K-b** | LOW | Kernel | `capability/table.rs` | Diagnostic `kprintln!` inside a live lock (latency hygiene). | Open |
| **XH-3** | LOW | Userspace | `xhci` | `spin()` discards success/failure; reset logs assert unverified facts. | Open |
| **XH-4** | LOW | Userspace | `xhci` | Poll drain never re-arms MSI-X (`irq_unmask`). | Open |
| **XH-5** | LOW | Userspace | `xhci` | Dead-BAR detection is diagnostic-only; falls through to enumerate. | Open |
| **DA1** | LOW | Documentation | `CLAUDE.md`/docs | Amendment shorthand (H1/P2/Phase C-D/Path C) had no decoder. | Fixed |

---

## 6. Regression check (prior fixes re-verified present in source)

```text
   Kernel     C1  ring-3 #GP kills task (not halt)          [ ✅ ]
              C2  ring-3 exception vectors kill (not halt)  [ ✅ ]
              C3  supervisor respawn non-panic              [ ✅ ]
              K1  bounded THRE poll in fault stubs          [ ✅ ]
              K3  spurious-vector iretq stub                [ ✅ ]
              V3  scheduler UAF Dekker handshake            [ ✅ ]
   Boundary   arch::imp seam, 0 leaks (4 independent checks) [ ✅ ]
   Userspace  M1/M2/M3/M5/M6/M8 · U3/U9-U12 · T1           [ ✅ ]  all present
              shell: 0 bare recv on a dependency            [ ✅ ]
              services: 0 unsafe (§18.2)                    [ ✅ ]
```

---

## 7. Documentation metrics

```text
   Grokability (cold, least-capable model)

     10 ┤
      9 ┤
      8 ┤
      7 ┤   ●───●           median 7/10  (reads: 7, 7)
      6 ┤
      5 ┤
        └────────────
          read A  read B

   Comprehension correctness  ████████████████████  maxed (all questions, both reads)
   Internal coherence         ██████████████████░░  9 / 10  (unanimous)
   Doc-vs-code agreement      ████████████████████  every spot-check matched
```

Audit-1 fix areas re-confirmed legible to a cold model: `log_fmt` · GRANT three-checks (§8.5) ·
stale-handle rule (§14.3) · `anti-patterns.md` location · `#[no_mangle]` gotcha.

---

## 8. Source of truth

This report is a derived view. The authoritative records are:

- `docs/kernel-audit.md` (Audit 4) - kernel findings, evidence, fixes.
- `docs/userspace-audit.md` (Audit 3) - userspace findings, evidence, fixes.
- `docs/documentation-audit.md` (Audit 2) - documentation findings + method.
- `docs/anti-patterns.md` - the field guide the documentation audit maintains.

On any disagreement between this report and a living doc, the living doc wins.
