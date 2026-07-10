# USB Hub Enumeration (xHCI)

**Status:** Design + in-progress, on `feat/dell-wyse-5070-goldmont-plus`. Trails `CLAUDE.md` (§12
drivers); does not amend it. The EHCI side already does this (`services/ehci/src/main.rs`); this doc
is the plan to give the xHCI driver the equivalent, adapted to xHCI's very different addressing model.

---

## 1. Why

The Dell Wyse 5070 (Goldmont Plus) routes its **back** USB ports through internal **Realtek hubs**
(`0x0bda`, PID `0x5411`/`0x5415` on the USB2 side, `0x0411`/`0x0415` on the USB3 side). On first boot
a keyboard on a back port was **invisible**: the xHCI driver found the hubs, saw each had "no
interrupt-IN endpoint (not a keyboard/mouse)", and did **not** recurse into them. Moved to a **front**
(direct root-hub) port, the same keyboard enumerated and worked.

The T630 got away without xHCI hub support because its keyboard came up on **EHCI**, whose driver
*does* enumerate through a hub (the "keyboard is behind a hub -> E3b enumerates it" path). The Wyse is
**xHCI-only** with hubs in the path, so the xHCI driver must learn to walk a hub itself. This is a
prerequisite for a usable keyboard on the machine's primary (back) ports.

---

## 2. The EHCI reference (what "walk a hub" means)

`services/ehci/src/main.rs` does it in two phases (`enumerate_hub`, `scan_devices`):

1. **Bring the hub up as a device:** get its device descriptor (`bDeviceClass == 0x09` = Hub),
   `Set_Address`, `Set_Configuration`, then `Get_Descriptor(Hub, type 0x29)` for the **downstream
   port count**.
2. **Walk the ports:** `Set_Feature(PORT_POWER)` each downstream port and let power settle; then per
   connected port, `Set_Feature(PORT_RESET)` -> `Clear_Feature(C_PORT_RESET)`, read the device + config
   descriptors, and if it is a boot-HID interface (class 3, protocol 1 keyboard / 2 mouse) bind its
   interrupt-IN endpoint.

The **USB hub class requests** above (`Get_Descriptor(Hub)`, `Set_Feature(PORT_POWER/PORT_RESET)`,
`Clear_Feature`, `Get_Status`) are standard control transfers over the hub's EP0 - **identical** on
xHCI. They port over unchanged; only the transport (how a control transfer is issued) differs.

---

## 3. Why xHCI is not a port of the EHCI code

The **addressing** of a device *behind* a hub is completely different, and this is the whole job.

- **EHCI** reaches a low/full-speed device behind a high-speed hub with **split transactions**: the
  hub address + downstream port are encoded straight into the qTD (`Ep::low(addr, mps, hub_addr,
  port)`), and the controller performs the split. Addressing is just metadata on each transfer.
- **xHCI** has no per-transfer split. Instead the controller is **told the topology** and routes
  internally:
  - The **hub's** Slot Context gets a **Hub** bit, a **Number of Ports** field, and **TT Think Time**
    (set with a Configure/Evaluate Context command after the hub is configured).
  - Each **downstream device's** Slot Context carries a **Route String** (the path of hub-port numbers,
    one nibble per tier), the **Root Hub Port Number** (the root port the whole chain hangs off), its
    **Speed**, and - for a low/full-speed device behind a high-speed hub - the **parent hub's Slot ID +
    Port Number** so the controller drives that hub's TT on the device's behalf.

So the reusable part is the hub *class requests*; the new part is building **route-string-addressed
slot contexts** for downstream devices.

---

## 4. Slot Context fields (the exact target)

Confirmed against the existing `enumerate_one` (`services/xhci/src/main.rs`), which already writes the
Input Slot Context for a root-port device:

```
islot dword0:  [31:27] Context Entries   [26] Hub   [23:20] Speed   [19:0] Route String
islot dword1:  [31:24] Number of Ports   [23:16] Root Hub Port Number
islot dword2:  [31:22] Interrupter Target  [17:16] TT Think Time
               [15:8]  TT Port Number      [7:0]   TT Hub Slot ID
```

Today the driver writes `dword0 = (1<<27) | (speed<<20)` (route 0, not a hub) and `dword1 = port<<16`.
The hub work sets the **Hub bit / Number of Ports / TTT** on a hub, and a **non-zero Route String +
Root Hub Port + parent TT** on a downstream device. Same writes, more fields.

---

## 5. Design

### 5.1 Refactor `enumerate_one` into shared pieces

Root ports and hub-downstream ports must share one addressing path:

- `address_device(route, root_port, speed, parent_slot, parent_port, dev_idx) -> Option<slot>`
  Enable Slot -> build the Input Context (route string, speed, root port, parent-TT fields) -> Address
  Device. The **root-port** caller passes `route = 0, parent_slot = 0`; the **downstream** caller passes
  the real route + parent. This is today's `enumerate_one` body, parameterized.
- `classify(slot, dev_idx) -> DevKind` where `DevKind = Hid{..} | Hub{nports} | Other`.
  Read the device descriptor; `bDeviceClass == 0x09` -> Hub (after Set_Configuration + reading the hub
  descriptor for `nports`); else walk the config descriptor for a boot-HID interrupt-IN endpoint.

### 5.2 The hub walk (new)

When `classify` returns `Hub`:

1. `Set_Configuration`, then a **Configure/Evaluate Context** command setting the slot's **Hub bit**,
   **Number of Ports**, and **TTT**.
2. `Get_Descriptor(Hub, 0x29)` -> `nports`; `Set_Feature(PORT_POWER)` each downstream port; settle
   (the spec's power-on-to-power-good time, a bounded `spin`/`delay`).
3. Per connected downstream port (`Get_Status`): `Set_Feature(PORT_RESET)` ->
   `Clear_Feature(C_PORT_RESET)` -> `Get_Status` for the device's **speed**, then
   `address_device(route = parent_route | (port << 4*(tier-1)), root_port, speed, parent_slot = hub
   slot, parent_port = port, ...)` -> `classify`:
   - **Hid** -> bind it (into a `MAX_HID` device slice, exactly as a root-port HID today).
   - **Hub** -> **recurse** (bounded depth; the route string extends by one nibble).
   - **Other** -> skip (Disable Slot).

### 5.3 Bounds and resources (§26.6)

- **Recursion depth is capped** (a route string is 5 tiers by spec; we cap lower, e.g. 3) so a
  malicious or looped topology cannot recurse without bound.
- **DMA slices** for enumeration come from a **fixed pool** (the hub needs a transient slice, downstream
  devices need one each during enumeration; bound HIDs keep theirs). No heap (§26.6.1).
- **Slots** taken for hubs / non-HID downstream devices are either kept (a hub in use) or Disable-Slot'd;
  the driver's existing **per-pass full controller re-init** frees everything each hot-plug pass, so any
  leak is bounded to one pass.

### 5.5 Constraints found while mapping the driver (refine 5.1-5.3)

Three facts the initial plan glossed, discovered reading `services/xhci/src/main.rs`:

1. **Only two device slices exist today.** The DMA arena lays out device slices as
   `DEV_BASE (0x7000) + i*DEV_STRIDE (0x4000)` up to the scratchpad at `0x10000` - that is exactly
   `MAX_HID = 2` slices. A hub walk needs the hub's slice **plus** each downstream device's slice at the
   same time (hub + up to MAX_HID bound HIDs + one transient = ~4). So the full walk requires **growing
   the xhci DMA arena** (a spawn-time page count) and a small **slice allocator** (fixed pool, §26.6),
   not just reusing the two.
2. **EP0 ring uses fixed linear offsets, no wrap.** Control transfers are placed at hand-picked EP0-ring
   offsets (0, 48, 96, 128) and the ring never wraps. A hub's per-port power/reset/status transfers add
   up, but a normal 4-8 port hub still fits under the 256-TRB (one-page) ring, so the walk uses a
   **bounded linear cursor** (advance per transfer, log-and-skip if a hub is absurdly large) rather than
   full ring-wrap + cycle-bit machinery.
3. **`enumerate_one` returns one `Hid`, a hub yields several.** The main loop calls `enumerate_one` per
   root port and appends its single result. The hub walk instead **appends multiple** bound HIDs into
   the loop's `devs[]`/`ndev`, so the shared enumerate path takes the device list by mutable ref rather
   than returning one device.

### 5.6 Increment 1 (landed): identify the hub

Before the arena/slice work, the driver now **identifies** a hub rather than dismissing it: it captures
`bDeviceClass`, and when a device has no interrupt-IN endpoint but is class `0x09`, it `Set_Configuration`s
it and reads the hub descriptor (`Get_Descriptor(Hub 0x29)`) to log the downstream-port count. This
proves the hub class-request path works over the shared `control()` helper and stages the walk; downstream
enumeration is the next increment (needs 5.5 item 1).

### 5.4 Hot-plug

The xHCI main loop already re-initialises the controller and re-scans every port on every pass. A
device coming or going behind a hub is picked up by the **same full re-walk** - heavier than watching a
hub's port-status-change bit, but correct and consistent with the current model. Incremental hub-port
watching is a later optimisation, not part of this first cut.

---

## 6. Testing

- **QEMU first:** `qemu-xhci` + `-device usb-hub` + `-device usb-kbd` attached behind the hub exercises
  the route-string addressing and the hub-class requests without hardware.
- **Then the Wyse:** the real Realtek hubs on the back ports, with a keyboard. Note the Wyse hubs are a
  **compound USB2 + USB3** device; the keyboard hangs off the **USB2** hub (`0x5411`/`0x5415`), so that
  is the tree the keyboard path walks (the USB3 side is handled the same way for a SuperSpeed device).

---

## 7. Risks

- The **slot-context bit-packing** (route string nibbles, parent-TT fields) is the fiddly part: a wrong
  field makes Address Device fail with a context/parameter error. The existing `enumerate_one` slot-context
  writes are the template, so it is contained, but it wants careful QEMU iteration before the Wyse.
- **Compound hub speeds:** a low/full-speed keyboard behind a high-speed hub needs the parent-TT fields;
  a SuperSpeed device behind a SuperSpeed hub does not. The walk keys off the downstream port's reported
  speed.

---

## 8. Relationship to the constitution

No new kernel surface: this is entirely within the userspace `xhci` driver (§12, unsafe-free via the
SDK `Mmio`/`Dma` wrappers, §18.1). It reads more of the same MMIO/DMA the driver already owns and issues
more of the same commands. The kernel routes the driver's IPC exactly as before.
