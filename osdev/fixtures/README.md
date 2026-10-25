# osdev/fixtures

Files a QEMU suite bakes into a test disk, and packages made for the hardware cards (`kind-dongle`, and `kind-rtl`'s
real use). Not built here; each says how it was made, so it can be made again.

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

## `dep-store-0.1.0.gsp` and `dep-user-0.1.0.gsp`

Two UNPUBLISHED package files for `osdev test deps` (`docs/developer-services-design.md` 16), signed by the same
throwaway test key as `hello-utility-0.9.0.gsp`, for `x86_64` only. `godspeed/dep-store` is a service that says it
started and then says each line a dependent sends it; `godspeed/dep-user` is a service whose manifest REQUIRES
`godspeed/dep-store ^0.1.0` (format 2) and whose contract names it as its one send peer: at start it sends it
`hello from dep-user`. So the suite's log shows the dependency both started first and reached.

Their projects (`godspeed.toml`, the contract and `src/main.rs`) are kept beside the CLI's examples, outside this
tree; each is the `hello-service` template with that behaviour. `dep-user`'s `godspeed.toml` carries

```toml
[requires]
"godspeed/dep-store" = "^0.1.0"
```

and its contract `ipc_send = ["godspeed/dep-store"]`. Made with the CLI that reads `[requires]` (godspeed-cli
`feat/dependencies`), as the file above:

```text
GODSPEED_SIGNING_KEY=<the seed> GODSPEED_OS=<this checkout> godspeed package --arch x86_64 --local
```

## `kind-service-0.1.0.gsp`, `kind-utility-0.1.0.gsp`, `kind-app-0.1.0.gsp` and `kind-driver-0.1.0.gsp`

Four UNPUBLISHED package files for `osdev test kinds`: one of every kind a developer makes, each exactly as the CLI
scaffolds it - nothing edited - signed by the same throwaway test key as `hello-utility-0.9.0.gsp`, for `x86_64` only.
The suite installs each from its file and sees it do its job: the service starts, the utility answers `hello, Ada`
and pipes, the app holds the screen until a key, and the driver drives QEMU's `edu` through `gs::driver`
by its registers, its interrupt and its DMA arena (`kind-driver: device id 0x010000ed, alive true, 5! = 120 by
interrupt, 64 bytes through DMA and back intact`). The service, utility and app were made 2026-10-22 with the CLI's
templates as conformed to `gs::driver` that day (`godspeed-cli` `feat/dependencies`); the driver was made again
2026-10-23 from the template that asks for an interrupt and an arena (`godspeed-cli` `feat/driver-packages`). Each
against this checkout:

```text
GODSPEED_OS=<this checkout> godspeed new <kind> kind-<kind> --publisher godspeed
cd kind-<kind>
GODSPEED_SIGNING_KEY=<the seed> GODSPEED_OS=<this checkout> godspeed package --arch x86_64 --local
```

Remake them when a template changes, or when the OS's program layout does.

## `kind-nic-0.1.0.gsp`

An UNPUBLISHED driver package for `osdev test driver-packages`, signed by the same throwaway test key: a driver that TAKES A
DEVICE OVER from GodspeedOS. Made 2026-10-23 with `godspeed new driver kind-nic --publisher godspeed`
(`godspeed-cli` `feat/driver-packages`) and changed in two places: its contract names the ethernet class
(`hw_pci_class = "020000"`, `hw_pci_bar = "auto"`, no interrupt and no arena), which `nic-driver` drives, and its
`src/main.rs` reads the e1000's status and its MAC (`RAL0`/`RAH0`) instead of `edu`'s registers, logging
`kind-nic: took the ethernet device over - e1000 (8086:100e), status ..., MAC 52:54:00:12:34:56`. Remade the same day
to know the RTL8168 too, by the NIC identity the kernel reports (`gs::driver::device::nic_vendor_device`): there it reads
the MAC from IDR0-5 and the PHY status, so the same file takes the NIC over on the T630 and the Wyse. It only reads,
so `nic-driver` meets the device as it left it. The suite sees the supervisor stand
`nic-driver` down, keep it down, and start it again once the package is uninstalled. Packaged as the files above.

## `kind-usb-0.1.0.gsp`

An UNPUBLISHED driver package for `osdev test driver-packages`, signed by the same throwaway test key: a driver of a
USB DEVICE, which has no registers of its own and is reached through its host. Made 2026-10-23 with `godspeed new
driver kind-usb --publisher godspeed` (`godspeed-cli` `feat/driver-packages`) and changed in two places: its contract
names QEMU's FTDI serial adapter by `hw_usb = "0403:6001"` (no PCI class, interrupt or arena), and its `src/main.rs`
uses `gs::driver::bus` - the device's identity from its host, `GET_DESCRIPTOR(DEVICE)` by a control transfer, a line
sent by bulk OUT, and the bulk IN asked (bounded at 10 s, after logging `kind-usb: listening ...`) until the adapter
has received something - logging `kind-usb: drove 0403:6001 through xhci - device release 0x0400, 38 bytes sent by
bulk OUT ok, bulk IN <n> bytes: "<what arrived>"`. The suite puts the adapter's serial port on a TCP socket, writes
`hello from the serial port` to it once the package listens, and reads back what the package sent. Packaged as the
files above.

## `kind-stick-0.1.0.gsp`

An UNPUBLISHED driver package for `osdev test driver-packages`, signed by the same throwaway test key, for `x86_64`
only: a driver of a USB device behind GodspeedOS's USB 2.0 host, `ehci`. Made 2026-10-23 from a copy of `kind-usb`
whose contract names QEMU's USB mass-storage device, `hw_usb = "46f4:0001"`, and whose `src/main.rs` sends it a SCSI
INQUIRY by the bulk-only transport through `gs::driver::bus` - the command block by bulk OUT, then the 36-byte answer
and the status by bulk IN, each asked again (5 ms apart, for up to 5 s) until it comes, since `ehci` answers "nothing
yet" rather than waiting - logging `kind-stick: drove 46f4:0001 through ehci - INQUIRY 36 bytes: vendor "QEMU",
product "QEMU HARDDISK"; status 0`. A stick is not vendor-specific, so a real `ehci` does not bind it: the suite
builds `ehci` with `bind-any-class`, and no image does. Packaged as the files above.

## `kind-rtl-0.1.0.gsp`

An UNPUBLISHED driver package for a HARDWARE card on the T630 and the Wyse, signed by the same throwaway test key, for
`x86_64` only: a driver that takes the Realtek RTL8168 over from `nic-driver` and uses all three grants on it - its
registers, its interrupt and a DMA arena - without sending or receiving a frame. Made 2026-10-23 from a copy of
`kind-nic` whose contract adds `hw_pci_irq = true` and `hw_dma = "8KiB"`, and whose `src/main.rs` stops the chip
(interrupts masked, receive and transmit off) and resets it (`CR.RST`), as `nic-driver` does, then, with the
configuration registers unlocked (`9346CR`), points its receive ring and both transmit rings at rings of one
descriptor the host owns in its arena, raises the chip's forced software interrupt (`TxPoll.FSWInt`, `SWInt`) with
the transmitter on and waits for it, then has the chip dump its tally counters into the arena by DMA (`DTCCR`), with
a guard after them that must stay untouched, logging one line per step: `kind-rtl: step 1 of 4 - stopped and
reset done; MAC ..., link ...`, `step 2 of 4 - rings in this arena: TNPDS ... THPDS ... RDSAR ..., arena ..., tally at
...`, `step 3 of 4 - software interrupt raised and taken (ISR ...)` and `step 4 of 4 - tally by DMA written (N of 64
bytes changed): TxOk ... RxOk ... missed ..., the 64 bytes after it untouched`. When no interrupt comes it says whether the chip latched `SWInt` (the interrupt was not
delivered) or did not. Rebuilt 2026-10-25 after the first T630 card: that one wrote `TxPoll` with the transmit rings
still pointing at the stood-down `nic-driver`'s memory, and the confined chip read outside its arena (refused by the
IOMMU), and its interrupt did not come. Rebuilt again the same day after the second: with the rings in its arena
the tally was written (TxOk 27790), but the chip still read outside the arena once, at another address, and `ISR`
never showed `SWInt` - so it now resets the chip and writes the ring addresses unlocked, and reads them back and
prints `ISR`, so the next card shows whether they took. They took, and the chip read `0x99300` again, so it was
rebuilt a third time: one line per step (0 given the chip, 1 stopped and reset, 2 rings, 3 the interrupt, 4 the
tally, then done), each followed by a two-second pause, so the IOMMU's fault report, which comes a few hundred
milliseconds late, falls after the step that caused it. On the T630 the arena is the IOMMU-confined one. On anything but an
RTL8168 it says so and touches nothing, which is all `osdev test driver-packages` sees of it on QEMU's e1000 (no MSI,
so no interrupt is routed there either). Packaged as the files above.

## `kind-dongle-0.1.0.gsp`

An UNPUBLISHED driver package for a HARDWARE card, not a QEMU suite (QEMU has no RTL8188CUS), signed by the same
throwaway test key, for all four architectures: a driver that takes a USB DEVICE over from GodspeedOS. Made
2026-10-23 from a copy of `kind-usb` (above) whose contract names the Realtek RTL8188CUS WiFi dongle,
`hw_usb = "0bda:8176"`, which `wifi-usb` drives, and whose `src/main.rs` reads, through the dongle's host, its
identity, its device descriptor and the two registers `wifi-usb` reads first (`SYS_CFG`, `ISO_CTRL`, by vendor
request 0x05), logging `kind-dongle: took 0bda:8176 over through <host> - device release ..., SYS_CFG ... ISO_CTRL
...: an RTL8188C, cut <c>, made by <maker>`. It only reads. On the card: `wifi-usb` is stood down at install and
started again at uninstall, and the radio comes back. Packaged as the files above, with no `--arch`.

## `gts-root-r4.der`

Google Trust Services' **GTS Root R4**, the ECDSA P-384 root that `speed.cloudflare.com`'s chain reaches, for
`osdev test download`: the suite puts it beside the registry's root so `godspeed check <url>` can measure a
download of any size from that server. A public certificate, nothing secret. Fetched from `https://i.pki.goog/r4.crt`;
its SHA-256 fingerprint is `34:9D:FA:40:58:C5:E2:63:12:3B:39:8A:E7:95:57:3C:4E:13:13:C8:3F:E6:8F:93:55:6C:D5:E8:03:1B:3C:7D`.
