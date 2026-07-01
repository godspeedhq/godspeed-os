# The System, Running

Every image on this page is a capture of the real GodspeedOS, booted in QEMU with an emulated
framebuffer and photographed straight from the guest's video memory (`build/fb_shot.py` drives QEMU
and grabs the framebuffer over the monitor). Nothing here is a mockup.

## Boot to steady state

The kernel comes up on all cores, spawns the supervisor directly, and the supervisor brings the
system to a multi-core steady state. The framebuffer console mirrors the serial log.

![GodspeedOS booting to steady state](images/boot.png)

<!--
More captures to add (each a real run, driven the same way):

- The `gsh>` shell prompt, live.
- `observe` - the live per-service view (cores, state, memory, restarts, queue depth).
- `drives` - the persistent GSFS volume, mounted.
- An `edit` session in the full-screen editor.
- The money shot: `chaos max-carnage` mid-storm, services dying and the supervisor respawning them,
  the kernel still alive.

These need the guest driven to the right state (keystrokes over the QEMU monitor / serial) before
the screendump; the boot capture above is the proof-of-concept that the pipeline works end to end.
-->
