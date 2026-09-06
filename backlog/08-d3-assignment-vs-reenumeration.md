# 8. D3: the assignment / re-enumeration split, and "cost 2"

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
