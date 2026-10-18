# osdev/fixtures

Files a QEMU suite bakes into a test disk. Not built here; each says how it was made, so it can be made again.

## `hello-utility-0.9.0.gsp`

An UNPUBLISHED Godspeed package file (`docs/developer-services-design.md` 15.6), for `osdev test gsp`: the
`godspeed/hello-utility` template at version 0.9.0 - a version the registry does not have, so it is never
confused with the published 0.2.0 - for `x86_64` only, signed by a THROWAWAY TEST KEY.

The key's secret half is in this file in plain sight on purpose: it is a fixture, and a machine trusts it only
if its operator runs `godspeed trust add godspeed <its public key>`, which the suite does and then undoes.

```text
seed (secret):  7e577e577e577e577e577e577e577e577e577e577e577e577e577e577e577e57
public key:     ed25519:7721d3618c7946587c59ce0693e08876368b5cc371f06105953e68d1a1396b3d
```

Made with the developer CLI from a copy of `hello-utility` whose contract and `Cargo.toml` say `0.9.0`:

```text
GODSPEED_SIGNING_KEY=<the seed> GODSPEED_OS=<this checkout> godspeed package --arch x86_64 --local
```

Remake it when the OS's program layout changes (the shared check refuses a package built before
`LAYOUT_RELEASE`); the suite will say so if it is stale.
