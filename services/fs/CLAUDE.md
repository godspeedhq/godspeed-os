# services/fs/

Filesystem service. TCB member in v1 (§6.1). **Non-restartable in v1.**

## Why it's in the TCB (v1)

fs owns persistent state for the system (§15). It cannot persist its own metadata to itself; the block driver holds a direct hardware capability for metadata storage. v2 goal: transactional metadata recovery so fs becomes restartable (§6.3).

## Dependencies

- `block-driver`: all I/O goes through block-driver IPC.
- `registry`: registers its endpoint so supervisor and other services can find it.

## v1 scope

- Flat namespace (no subdirectories beyond what supervisor needs).
- Read/write files by path.
- Serves: `supervisor` (service binaries), and any service holding `ipc_send = ["fs"]`.

## Exposed interface (via IPC)

| Request      | Args              | Response |
|--------------|-------------------|----------|
| `ReadFile`   | path (string)     | file bytes or `NotFound` / `IoError` |
| `WriteFile`  | path, data bytes  | `Ok` or `IoError` |
| `StatFile`   | path              | size, exists flag |

## State and persistence (§15)

fs holds an in-memory inode table built from the on-disk superblock at mount time. All writes go immediately to block-driver. There is no write-back cache in v1.

The filesystem cannot recover from a crash that leaves a partial write in progress. v1 accepts this: both block-driver and fs are non-restartable. The only recovery mechanism is a full system reboot.
