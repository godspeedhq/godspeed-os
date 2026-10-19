# 92. An app cannot be sent to the background, so nothing can tell it a newer version is installed

**Status: OPEN, 2026-10-17.** A design recorded at the operator's request, not a defect.

## What is true today

An installed `app` (`gs::app`) owns the console while it runs: its keys are its own, and the shell waits until it
ends. So:

- **It cannot be backgrounded.** `background` covers shell built-ins only (`utilities/55_background.md`), and the
  `[b] background` key works on the shell's own jobs; an app holds the keyboard, so the shell never sees the key.
- **It cannot be upgraded while open.** `godspeed upgrade` is typed at the shell, which is waiting on the app. The
  upgrade happens after the app is closed, and its next launch is the new version.
- **So an "a newer version is installed" notice has no one to deliver it.** It is recorded as not built in
  `docs/developer-services-design.md` 14.2 for that reason.

`utilities/55_background.md` says "applications do not need this". That was written when an application meant a
service, before the `app` kind existed, and is stale for apps.

## What is RULED OUT

- **Closing an open app from outside to upgrade it** (a `force`, or the supervisor stopping it). Only the app knows
  whether it holds unsaved work, so only the app may decide when to restart.
- **A setting to opt in.** Every piece below comes from `gs::app` with nothing declared.

## The design: three pieces, in this order

1. **Background and foreground.** `gs::app` reserves one key, Ctrl+Z. The app gives the console back and keeps
   running; the shell draws a prompt and lists it in `jobs`. `foreground <job>` tells the app to take the console
   again, and it redraws. An app may refuse while it is mid-save. While in the background it gets no keys and does
   not draw, as `copier` stays off the prompt. The kernel already has what this needs: the console foreground taken
   and released (syscall 40), and given back when its holder dies.
2. **The notice.** With an app in the background the prompt is free, so `godspeed upgrade` can run while it is open.
   An upgrade then leaves the running app alone, records the new version for its next launch, and tells it; the app
   offers "restart to update" when it is brought back, as Chrome and VS Code do.
3. **The relaunch.** The app ends itself with a status that means "start me again", and the shell runs the new
   version with the same arguments, so the user does not retype the command.

## The next concrete step

Piece 1: Ctrl+Z and `foreground` for apps, in `gs::app` and the shell, with a QEMU check that an app sent to the
background keeps running, the prompt answers, and `foreground` brings it back. Pieces 2 and 3 follow on it.
