# 8. D3: the assignment / re-enumeration split, and "cost 2"

**Status: CLOSED 2026-10-29.** Both decisions were made and built: `docs/service-ownership.md` Slice 2 (the kernel derives BAR and IRQ from a BDF) and Slice 4 (the assignment / re-enumeration split), with Slice 3 finished by `39746569`. That doc keeps the one caveat: the already-programmed-BAR branch has not run on hardware.

**Status before 2026-10-29: OPEN - a DECISION, not work.** The data gate is met and both walks agree on every
machine available; what remains is the split below, which nobody has chosen between.

**Severity:** design decision, blocking the D3 gate.
**Status:** the DATA gate is met; what remains is a decision nobody has made.

## Where it stands

`docs/service-ownership.md` records the D3 work in full and reaches this point:

> So the gate D3 was waiting on is OPEN: the two walks agree on every machine available, which is
> what "record, cross-check, and only then switch over" asked for. What remains for D3 is design,
> not data - see the assignment/re-enumeration split below, and cost 2, which is still unresolved.

So the cross-check has done its job: the userspace walk and the kernel walk agree on every machine
in the fleet. The blocker is not evidence, it is an unmade decision about how assignment and
re-enumeration divide, and what "cost 2" should be.

## Why this file exists rather than a longer one

The design narrative belongs in `docs/service-ownership.md` and is not copied here - that document
is the single source for the reasoning, including the Wyse false-`DISAGREES` and the `00:0d.2`
function-0 bug that the per-device diff caught.

What this file owns is the **status**: that D3 is blocked on a decision, not on more data, so that
someone scanning open work sees it without reading a design narrative to its end.

## Next step

Read `docs/service-ownership.md` from the D3 section, make the assignment/re-enumeration call, and
settle cost 2. Then either close this file or record the decision in it.
