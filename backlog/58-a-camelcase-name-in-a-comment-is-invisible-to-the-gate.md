# 58 - a CamelCase name in a comment is invisible to `comment_symbol_check`

**Status:** OPEN - a measured blind spot in a gate that shipped the same day. Recorded rather than
closed because closing it is a triage pass over 23 sites, not a regex change, and doing it in the
commit that added the gate would have meant baselining findings to keep the gate green.
**Found:** 2026-09-26, by the kernel half of the first comment sweep, which hit a dead name the new
gate could not see.

## What is not checked

`scripts/comment_symbol_check.py` asks whether a backticked name in a Rust comment exists in the
code. It matches two shapes:

- `TOKEN` - snake_case or SCREAMING_CASE, and it **requires an underscore**, because a single bare
  word in a comment is prose far more often than it is an identifier.
- `PATH_TOKEN` - a path-qualified citation, checked on its final segment (`control::process_pending`).
  Added immediately, because it was cheap: 5 names over 8 sites and nothing to baseline.

A **CamelCase** name matches neither. So a type or an enum variant cited in a comment is never
checked at all.

## The case that found it

`kernel/src/arch/arm/mod.rs` carried, in the present tense:

```
// exclusion is a PROTOCOL instead - `dwc2::hotplug_poll` takes `UsbExclusive` and every other
// shared-selection path stands aside for the duration
```

Both names were dead - they went with `arch/arm/dwc2.rs` in the slice-5 USB move - and the sentence
described a mechanism that had not existed for a month. `dwc2::hotplug_poll` is caught by
`PATH_TOKEN` now. **`UsbExclusive` is still not caught by anything**, and if the path-qualified half
had been written differently the whole clause would have stayed invisible.

## Measured cost of turning it on, which is why it is not on

A CamelCase pattern requiring two or more humps finds **15 names over 23 sites**. Roughly half are
not ours and never will be, and belong in the baseline permanently exactly as the register names
already there do:

| Not ours | What it is |
|---|---|
| `AttrIndx`, `DminLine`, `IminLine` | ARM system-register FIELDS (`MAIR_EL1`, `CTR_EL0`) |
| `HubAddr`, `PrtAddr`, `SplEna` | DWC2 `HCSPLT` register fields |
| `GenuineIntel` | the CPUID vendor string |

The other half look like real rot and want reading one at a time: `SetClock`, `CreateEndpoint`,
`UnknownSyscall`, `ReclaimBuffer`, `UsbExclusive`, and three `Write*` names in `services/shell`.

## Why it is filed rather than done

Enabling the pattern and seeding the new names into the baseline in one step would put **findings**
into the baseline to keep the gate green. That is the one thing a ratchet must not be used for: the
baseline's whole value is that everything in it has been looked at and judged an outward reference.
Half of these have not been looked at.

So the work is: enable `CAMEL`, then triage all 23 sites the way the 68 snake_case ones were - FIX
where a comment points at live code that is not there, KEEP and baseline where it records a removal
(CLAUDE.md 26.7) - and only then commit, with the baseline honest.

`build/probe_token_widening.py` is the measurement, and re-running it regenerates the list.

## A second, narrower gap in the same instrument

`TOKEN` requires an underscore, so a single-word snake_case identifier (`send`, `recv`, `spawn`) is
also unchecked. That one is deliberate and should stay: those words are prose constantly, and the
false-positive rate would make the gate unusable. It is noted here only so the next reader knows the
underscore rule is a decision and not an oversight.
