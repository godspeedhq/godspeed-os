# Milestone 3 — Scheduler (Single Core)

> Per-core run queue, context switching, and timer preemption working on one core.

## Task Structure

- [ ] `Task` struct: `TaskContext`, stack, state (`Ready`/`Running`/`Blocked`/`Dead`)
- [ ] Stack allocation per task (from frame allocator)
- [ ] Initial `TaskContext` set up so first `switch_context` jumps to entry point

## Run Queue

- [ ] Per-core `RunQueue`: round-robin over `Ready` tasks
- [ ] `enqueue(task)` / `dequeue() -> Task`
- [ ] `scheduler::run()` loop: pick next ready task, call `switch_context`

## Context Switch

- [ ] `switch_context` saves/restores callee-saved registers + CR3
- [ ] Verified: switching between two tasks executes both

## Preemption

- [ ] Local APIC timer fires every 10 ms (§9.1 quantum)
- [ ] Timer IRQ handler calls `scheduler::tick()` which preempts if quantum expired
- [ ] A tight-loop task does not starve another task on the same core

## Acceptance

- Two tasks on core 0 both make progress (serial output from each) over a 1 s window
- Removing explicit yields does not stop the second task from running
