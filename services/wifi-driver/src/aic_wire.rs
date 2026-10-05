// SPDX-License-Identifier: GPL-2.0-only
//! The AIC8800D80's bytes on the wire, with no hardware and no dependencies: the bus header's CRC, a
//! host-to-chip frame, the patch table's groups, and the parameter blocks the firmware's messages carry.
//! `aic.rs` drives the bus; this says what goes over it.
//!
//! **Separate so it can be tested on the host.** The driver only builds for the board, and a wrong byte
//! offset in a message otherwise costs a flash to find. This file names nothing outside `core`, so it
//! compiles on its own:
//!
//! ```text
//! rustc --edition 2021 --test services/wifi-driver/src/aic_wire.rs -o build/aic_wire_test && build/aic_wire_test
//! ```
//!
//! The tests pin values the BOARD has confirmed (the header CRC the chip accepted, the load addresses the
//! patch table gave), and, for messages not yet sent, the layouts read from the vendor driver
//! (radxa-pkg/aic8800 @09c65d61), so a later edit cannot quietly move a field.

/// The bus header's type byte for a host-to-chip command.
pub const TYPE_CMD: u8 = 0x11;
/// The driver's task id, the source of every message (`DRV_TASK_ID`).
pub const DRV_TASK_ID: u16 = 100;
/// One SDIO block, the unit a frame is padded to.
pub const BLOCK: usize = 512;
/// The largest frame the vendor driver sends, `CMD_BUF_MAX`: three blocks.
pub const FRAME_MAX: usize = 1536;

pub fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// CRC-8 over the bus header's first three bytes: polynomial `0x07`, initial 0, as `crc8_ponl_107` writes
/// it. The D80 checks it; a wrong one is refused by the chip without a word.
pub fn crc8(bytes: &[u8]) -> u8 {
    let mut crc: u8 = 0;
    for &b in bytes {
        let mut i: u8 = 0x80;
        while i > 0 {
            if crc & 0x80 != 0 {
                crc = (crc << 1) ^ 0x07;
            } else {
                crc <<= 1;
            }
            if b & i != 0 {
                crc ^= 0x07;
            }
            i >>= 1;
        }
    }
    crc
}

/// One host-to-chip message as it goes on the bus (`rwnx_set_cmd_tx` + `aicwf_sdio_tx_msg`): the 4-byte
/// header `[len lo, len hi (4 bits), 0x11, crc8]` where `len` counts what follows it, a zero word, the
/// 8-byte message header `{id, dest, src, param_len}`, the parameters; then, if that is not a whole number
/// of 512-byte blocks, a 4-byte zero tail and zeros up to the next whole block. Returns the frame's length
/// in bytes, or `None` when it would not fit `FRAME_MAX`.
pub fn build_frame(id: u16, dest: u16, param: &[u8], frame: &mut [u8; FRAME_MAX]) -> Option<usize> {
    let len = 4 + 8 + param.len();
    let mut total = (4 + len + 3) & !3;
    if total % BLOCK != 0 {
        total = (total + 4).div_ceil(BLOCK) * BLOCK;
    }
    if total > FRAME_MAX || len > 0xfff {
        return None;
    }
    frame.fill(0);
    frame[0] = (len & 0xff) as u8;
    frame[1] = ((len >> 8) & 0x0f) as u8;
    frame[2] = TYPE_CMD;
    frame[3] = crc8(&frame[0..3]);
    let m = 8; // after the header and the zero word
    frame[m..m + 2].copy_from_slice(&id.to_le_bytes());
    frame[m + 2..m + 4].copy_from_slice(&dest.to_le_bytes());
    frame[m + 4..m + 6].copy_from_slice(&DRV_TASK_ID.to_le_bytes());
    frame[m + 6..m + 8].copy_from_slice(&(param.len() as u16).to_le_bytes());
    frame[m + 8..m + 8 + param.len()].copy_from_slice(param);
    Some(total)
}

/// The patch table's groups as `(name, type, pairs)`, walked as `aicbt_patch_table_alloc` walks them: a
/// 16-byte file tag, then `name[16], type u32, len u32, len * (addr u32, value u32)` to the end of the file.
pub struct Groups<'t> {
    t: &'t [u8],
    at: usize,
}

impl<'t> Groups<'t> {
    pub fn new(t: &'t [u8]) -> Option<Self> {
        if t.len() < 16 || &t[..12] != b"AICBT_PT_TAG" {
            return None;
        }
        Some(Groups { t, at: 16 })
    }
}

impl<'t> Iterator for Groups<'t> {
    /// `(name, type, the pairs' bytes)`.
    type Item = (&'t [u8], u32, &'t [u8]);
    fn next(&mut self) -> Option<Self::Item> {
        if self.at + 24 > self.t.len() {
            return None;
        }
        let name = &self.t[self.at..self.at + 16];
        let ty = le32(self.t, self.at + 16);
        let len = le32(self.t, self.at + 20) as usize;
        let start = self.at + 24;
        let end = start.checked_add(len.checked_mul(8)?)?;
        if end > self.t.len() {
            return None;
        }
        self.at = end;
        let name_len = name.iter().position(|&b| b == 0).unwrap_or(16);
        Some((&name[..name_len], ty, &self.t[start..end]))
    }
}

/// The transmit power table the vendor driver sends with no `aic_userconfig_8800d80.txt` - its
/// `userconfig_info` defaults, in `txpwr_idx_lvl_v3`'s layout: an enable byte, then per band and mode one
/// signed level per rate (`0x80` = not used). The message is the union of four layouts, so it is sent at
/// the largest one's size, 95 bytes, with the rest zero.
pub fn txpwr_lvl_v3() -> [u8; 95] {
    const ROWS: [&[u8]; 6] = [
        &[20, 20, 20, 20, 20, 20, 20, 20, 18, 18, 16, 16],
        &[20, 20, 20, 20, 18, 18, 16, 16, 16, 16],
        &[20, 20, 20, 20, 18, 18, 16, 16, 16, 16, 15, 15],
        &[0x80, 0x80, 0x80, 0x80, 20, 20, 20, 20, 18, 18, 16, 16],
        &[20, 20, 20, 20, 18, 18, 16, 16, 16, 15],
        &[20, 20, 20, 20, 18, 18, 16, 16, 16, 15, 14, 14],
    ];
    let mut p = [0u8; 95];
    p[0] = 1;
    let mut at = 1;
    for row in ROWS {
        p[at..at + row.len()].copy_from_slice(row);
        at += row.len();
    }
    p
}

/// `mm_set_rf_calib_req` with the defaults (no user configuration, no crystal trim): the 2.4 and 5 GHz
/// calibration masks, the alpha parameter, Bluetooth calibration off with its default parameter, then two
/// zero trim bytes - 22 bytes, sent at the struct's padded 24.
pub fn rf_calib() -> [u8; 24] {
    let mut p = [0u8; 24];
    for (i, v) in [0x0f8fu32, 0x0f0f, 0x0c34_c008, 0, 0x0026_4203].iter().enumerate() {
        p[4 * i..4 * i + 4].copy_from_slice(&v.to_le_bytes());
    }
    p
}

/// `ME_CONFIG_REQ`'s 112 parameter bytes as the vendor runtime driver sends them for the D80 with its build
/// defaults (`rwnx_send_me_config_req` after `rwnx_handle_dynparams`): one spatial stream, 80 MHz, HT, VHT
/// and HE on, every capability from the driver's static band tables. Field by field:
///
/// - `0..32` HT: capability `0x0963` (LDPC, 20/40, SGI 20 and 40, RX STBC 1, 7935-byte A-MSDU), A-MPDU
///   parameters `0x1f` (64 KiB, 16 us density), MCS 0-7 plus MCS32, highest rate 150, TX set defined;
/// - `32..44` VHT: capability `0x03987131`, MCS 0-9 on one stream (`0xfffe`) both ways, highest 390;
/// - `44..100` HE: MAC capabilities, PHY capabilities, MCS 0-11 on one stream at 80 MHz, PPE thresholds;
/// - `100` transmit lifetime 1000 ms, `102` widest bandwidth 80 MHz (2), then HT, VHT, HE supported, HE
///   uplink off, power save on, antenna diversity on, dynamic power save off.
pub const ME_CONFIG: [u8; 112] = [
    0x63, 0x09, 0x1f, 0xff, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x96, 0x00, 0x01,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x31, 0x71, 0x98, 0x03, 0xfe, 0xff, 0x86, 0x01, 0xfe, 0xff, 0x86, 0x01, 0x00, 0x00, 0x02, 0x00,
    0x00, 0x00, 0x06, 0xe0, 0x2b, 0x58, 0x0d, 0xc0, 0xcf, 0x00, 0x02, 0x30, 0x00, 0x00, 0xfe, 0xff,
    0xfe, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x38, 0x1c, 0xc7, 0x01, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0xe8, 0x03, 0x02, 0x01, 0x01, 0x01, 0x00, 0x01, 0x01, 0x00, 0x00, 0x00,
];

/// The channel list's transmit power, in dBm, on every channel: the 20 dBm the vendor driver's own world
/// regulatory domain (`regdom_00`) allows. The vendor driver's table says 30, and Linux's regulatory code
/// lowers it to the domain's 20 before the list is built; this driver has no regulatory code, so it states
/// the result. The lower of the two, which is the safe direction to be wrong in.
pub const CHAN_TX_POWER_DBM: u8 = 20;

/// `ME_CHAN_CONFIG_REQ`'s 254 bytes (`rwnx_send_me_chan_config_req`): 14 slots of 2.4 GHz channels then
/// 28 of 5 GHz, each `{u16 freq, u8 band, u8 flags, s8 tx_power, pad}`, then the two counts. 2.4 GHz is
/// channels 1-14; 5 GHz is the 25 the vendor's band table lists (5180-5320, 5500-5720, 5745-5825, 20 MHz
/// apart), sent only when the firmware said it does 5 GHz. No flags: the vendor's world domain disables
/// nothing and marks nothing passive or radar in this build.
pub fn chan_config(five_ghz: bool) -> [u8; 254] {
    let mut p = [0u8; 254];
    let mut put = |slot: usize, freq: u16, band: u8| {
        let at = slot * 6;
        p[at..at + 2].copy_from_slice(&freq.to_le_bytes());
        p[at + 2] = band;
        p[at + 3] = 0;
        p[at + 4] = CHAN_TX_POWER_DBM;
    };
    let mut n24 = 0usize;
    for ch in 1..=13u16 {
        put(n24, 2407 + 5 * ch, 0);
        n24 += 1;
    }
    put(n24, 2484, 0);
    n24 += 1;
    let mut n5 = 0usize;
    if five_ghz {
        let ranges: [(u16, u16); 3] = [(5180, 5320), (5500, 5720), (5745, 5825)];
        for (lo, hi) in ranges {
            let mut f = lo;
            while f <= hi {
                put(14 + n5, f, 1);
                n5 += 1;
                f += 20;
            }
        }
    }
    p[252] = n24 as u8;
    p[253] = n5 as u8;
    p
}

/// `MM_START_REQ`'s 72 bytes: sixteen zero PHY configuration words (the vendor's PHY configuration step is
/// compiled out, so they are zero there too), then a 300 ms U-APSD timeout and a 20 ppm low-power clock.
pub fn mm_start() -> [u8; 72] {
    let mut p = [0u8; 72];
    p[64..68].copy_from_slice(&300u32.to_le_bytes());
    p[68..70].copy_from_slice(&20u16.to_le_bytes());
    p
}

/// `MM_SET_COEX_REQ`'s 16 bytes as the default build sends them: Bluetooth on, coexistence null frames
/// left enabled, null-CTS on, no periodic timer, no time slots.
pub const COEX: [u8; 16] = [1, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];

/// `MM_ADD_IF_REQ`'s 10 bytes for a station (`MM_STA` = 0): the type, a pad byte, the interface's address
/// in wire order at offset 2, then `p2p` false and a pad byte.
pub fn add_if(mac: [u8; 6]) -> [u8; 10] {
    let mut p = [0u8; 10];
    p[2..8].copy_from_slice(&mac);
    p
}

/// `SCANU_START_REQ`'s 376 bytes for a wildcard scan of every channel in the list `chan_config` sends
/// (`rwnx_send_scanu_req`): the same channel entries in the first 252 bytes, one empty SSID (an SSID count of 1,
/// length 0 - every network answers), the broadcast BSSID, the interface's index, the channel count, and
/// no CCK suppression and no fixed dwell time (both 0). No extra information elements, so the vendor's
/// separate vendor-IE message is not needed.
pub fn scanu_start(vif: u8, five_ghz: bool) -> [u8; 376] {
    let mut p = [0u8; 376];
    let chans = chan_config(five_ghz);
    p[..252].copy_from_slice(&chans[..252]);
    p[352..358].copy_from_slice(&[0xff; 6]);
    p[366] = vif;
    p[367] = chans[252] + chans[253];
    p[368] = 1;
    p
}

/// One `SCANU_RESULT_IND` (`struct scanu_result_ind`): the frequency and band it was heard on, its signal in
/// dBm, and the whole 802.11 beacon or probe response it carried, MAC header included.
pub struct ResultInd<'a> {
    pub freq: u16,
    pub band: u8,
    pub rssi: i8,
    pub frame: &'a [u8],
}

/// Read a result indication's parameters: `{u16 length, u16 framectrl, u16 center_freq, u8 band, u8 sta_idx,
/// u8 inst_nbr, s8 rssi, pad[2], payload}`. `None` for one too short to hold a management frame's header and
/// fixed fields (36 bytes), or whose length runs past what arrived - the length comes from the air.
pub fn parse_result(params: &[u8]) -> Option<ResultInd<'_>> {
    if params.len() < 12 {
        return None;
    }
    let len = u16::from_le_bytes([params[0], params[1]]) as usize;
    if len < 36 || 12 + len > params.len() {
        return None;
    }
    Some(ResultInd {
        freq: u16::from_le_bytes([params[4], params[5]]),
        band: params[6],
        rssi: params[9] as i8,
        frame: &params[12..12 + len],
    })
}

impl ResultInd<'_> {
    /// The BSSID, `mgmt->bssid` (the third address, offset 16).
    pub fn bssid(&self) -> [u8; 6] {
        let mut b = [0u8; 6];
        b.copy_from_slice(&self.frame[16..22]);
        b
    }
    /// The capability field of a beacon or probe response (after the timestamp and the interval).
    pub fn capability(&self) -> u16 {
        u16::from_le_bytes([self.frame[34], self.frame[35]])
    }
    /// The information elements, from offset 36 to the end.
    pub fn ies(&self) -> &[u8] {
        &self.frame[36..]
    }
    /// The SSID element's body, if it is there and fits. Zero length is a hidden network.
    pub fn ssid(&self) -> Option<&[u8]> {
        let ies = self.ies();
        let mut at = 0usize;
        while at + 2 <= ies.len() {
            let (id, len) = (ies[at], ies[at + 1] as usize);
            if at + 2 + len > ies.len() {
                return None;
            }
            if id == 0 {
                return if len <= 32 { Some(&ies[at + 2..at + 2 + len]) } else { None };
            }
            at += 2 + len;
        }
        None
    }
}

/// The 802.11 channel number of a centre frequency in MHz: 1-13 from 2412, 14 at 2484, and 5 GHz channels
/// from 5000. 0 for anything else.
pub fn channel_of(freq: u16) -> u8 {
    match freq {
        2484 => 14,
        2412..=2472 => ((freq - 2407) / 5) as u8,
        5000..=5895 => ((freq - 5000) / 5) as u8,
        _ => 0,
    }
}

// --------------------------------------------------------------------------------------- join (V5)

/// The station-management task, and the join messages' ids (`TASK_SM` = 6, so `6 << 10` on), with the
/// key and control-port messages that follow a join. Each confirm is one past its request.
pub const TASK_SM: u16 = 6;
pub const SM_CONNECT_REQ: u16 = 0x1800;
pub const SM_CONNECT_IND: u16 = 0x1802;
pub const SM_DISCONNECT_REQ: u16 = 0x1803;
pub const SM_DISCONNECT_IND: u16 = 0x1805;
pub const MM_KEY_ADD_REQ: u16 = 0x0024;
pub const ME_SET_CONTROL_PORT_REQ: u16 = 0x1404;
pub const SM_CONNECT_CFM: u16 = 0x1801;
pub const SM_DISCONNECT_CFM: u16 = 0x1804;
pub const MM_KEY_ADD_CFM: u16 = 0x0025;
/// Its parameters are not read by the vendor driver and it has no struct in the source; only its arrival
/// is used here.
pub const ME_SET_CONTROL_PORT_CFM: u16 = 0x1405;
/// `{u8 sta_idx}`; the confirm is `struct mm_get_sta_info_cfm` (`rwnx_send_get_sta_info_req`).
pub const MM_GET_STA_INFO_REQ: u16 = 0x0075;
pub const MM_GET_STA_INFO_CFM: u16 = 0x0076;
/// `{u8 inst_nbr}` - what `rwnx_close` sends to take the interface down.
pub const MM_REMOVE_IF_REQ: u16 = 0x0008;
pub const MM_REMOVE_IF_CFM: u16 = 0x0009;

/// `SM_CONNECT_REQ`'s flags (`enum connection_flags`): the host runs the 802.1X port, and WPA/WPA2 is in
/// use. An open network sets neither (`rwnx_send_sm_connect_req` sets each from the request's crypto).
pub const CONTROL_PORT_HOST: u32 = 1 << 0;
pub const WPA_WPA2_IN_USE: u32 = 1 << 3;

/// `SM_CONNECT_REQ`'s 320 bytes (`rwnx_send_sm_connect_req`): the SSID, the AP's BSSID, its channel
/// (transmit power left 0, as the vendor's is), the flags - `CONTROL_PORT_HOST | WPA_WPA2_IN_USE` when `ie`
/// carries an RSN element (the host runs the handshake), none for an open network - the control port's
/// ethertype `88 8e` in network order, open-system authentication, U-APSD on the voice queue, the
/// interface, and the elements in `ie`. `None` for an SSID over 32 bytes or elements over the 256-byte
/// buffer.
pub fn sm_connect(ssid: &[u8], bssid: [u8; 6], freq: u16, band: u8, vif: u8, ie: &[u8]) -> Option<[u8; 320]> {
    if ssid.len() > 32 || ie.len() > 256 {
        return None;
    }
    let mut p = [0u8; 320];
    p[0] = ssid.len() as u8;
    p[1..1 + ssid.len()].copy_from_slice(ssid);
    p[34..40].copy_from_slice(&bssid);
    p[40..42].copy_from_slice(&freq.to_le_bytes());
    p[42] = band;
    let flags = if ie.is_empty() { 0 } else { CONTROL_PORT_HOST | WPA_WPA2_IN_USE };
    p[48..52].copy_from_slice(&flags.to_le_bytes());
    p[52..54].copy_from_slice(&[0x88, 0x8e]);
    p[54..56].copy_from_slice(&(ie.len() as u16).to_le_bytes());
    p[60] = 0x01;
    p[61] = vif;
    p[64..64 + ie.len()].copy_from_slice(ie);
    Some(p)
}

/// What `SM_CONNECT_IND` says about the association (`struct sm_connect_ind`): its 802.11 status (0 is
/// success), the AP's BSSID, and `ap_idx` - the firmware's station index for the AP, which the keys, the
/// transmit descriptor and the control port all name.
pub struct ConnectInd {
    pub status: u16,
    pub bssid: [u8; 6],
    pub vif: u8,
    pub ap_idx: u8,
    pub aid: u16,
    pub freq: u16,
}

pub fn parse_connect_ind(p: &[u8]) -> Option<ConnectInd> {
    if p.len() < 826 {
        return None;
    }
    let mut bssid = [0u8; 6];
    bssid.copy_from_slice(&p[2..8]);
    Some(ConnectInd {
        status: u16::from_le_bytes([p[0], p[1]]),
        bssid,
        vif: p[9],
        ap_idx: p[10],
        aid: u16::from_le_bytes([p[820], p[821]]),
        freq: u16::from_le_bytes([p[824], p[825]]),
    })
}

/// CCMP, as the firmware numbers ciphers.
pub const MAC_CIPHER_CCMP: u8 = 2;

/// `MM_KEY_ADD_REQ`'s 44 bytes for a CCMP key (`struct mm_key_add_req`): a pairwise key against the AP's
/// station index with key index 0, or a group key against `0xff` with the index the GTK came with.
pub fn key_add(key_idx: u8, sta_idx: u8, key: &[u8; 16], pairwise: bool, inst: u8) -> [u8; 44] {
    let mut p = [0u8; 44];
    p[0] = key_idx;
    p[1] = sta_idx;
    p[4] = 16;
    p[8..24].copy_from_slice(key);
    p[40] = MAC_CIPHER_CCMP;
    p[41] = inst;
    p[43] = pairwise as u8;
    p
}

/// `ME_SET_CONTROL_PORT_REQ`: `{sta_idx, open}`. Sent once the keys are in, which opens the port to data.
pub fn control_port(sta_idx: u8, open: bool) -> [u8; 2] {
    [sta_idx, open as u8]
}

/// `SM_DISCONNECT_REQ`: `{u16 reason, u8 vif}`, padded to 4.
pub fn disconnect(reason: u16, vif: u8) -> [u8; 4] {
    let r = reason.to_le_bytes();
    [r[0], r[1], vif, 0]
}

/// The RSSI the firmware reports for a station (`struct mm_get_sta_info_cfm`, `rssi` at byte 8, read as
/// a signed dBm value by `rwnx_cfg80211_get_station`). `None` for a confirm too short to hold it.
pub fn sta_info_rssi(cfm: &[u8]) -> Option<i8> {
    cfm.get(8).map(|&b| b as i8)
}

/// The bus header's type byte for a host-to-chip DATA frame.
pub const TYPE_DATA_TX: u8 = 0x01;

/// One data frame to the chip (`aicwf_sdio_aggr`, the non-DMA path): the 4-byte header `[len lo, len hi,
/// 0x01, crc8]` with `len` = the 28-byte host descriptor plus the payload, then the descriptor
/// (`hostdesc`: payload length, flags 0, host id, destination, source, ethertype in network order, queue,
/// TID, interface, the AP's station index, flags 0), then the payload - the 802.3 frame WITHOUT its
/// 14-byte Ethernet header - padded to 4, a 4-byte tail, and whole 512-byte blocks. `None` past
/// `FRAME_MAX`. Queue and TID are 0: an EAPOL frame is not IP and goes best-effort.
pub fn build_data_frame(dst: [u8; 6], src: [u8; 6], ethertype: u16, payload: &[u8], vif: u8, sta: u8, hostid: u32, frame: &mut [u8; FRAME_MAX]) -> Option<usize> {
    let len = 28 + payload.len();
    let mut total = (4 + len + 3) & !3;
    total = (total + 4).div_ceil(BLOCK) * BLOCK;
    if total > FRAME_MAX || len > 0xfff {
        return None;
    }
    frame.fill(0);
    frame[0] = (len & 0xff) as u8;
    frame[1] = ((len >> 8) & 0x0f) as u8;
    frame[2] = TYPE_DATA_TX;
    frame[3] = crc8(&frame[0..3]);
    let d = 4;
    frame[d..d + 2].copy_from_slice(&(payload.len() as u16).to_le_bytes());
    frame[d + 4..d + 8].copy_from_slice(&hostid.to_le_bytes());
    frame[d + 8..d + 14].copy_from_slice(&dst);
    frame[d + 14..d + 20].copy_from_slice(&src);
    frame[d + 20..d + 22].copy_from_slice(&ethertype.to_be_bytes());
    frame[d + 24] = vif;
    frame[d + 25] = sta;
    frame[d + 28..d + 28 + payload.len()].copy_from_slice(payload);
    Some(total)
}

/// A received data packet's hardware header, before the 802.11 frame: `struct hw_rxhdr` (56 bytes) and
/// the 4 the SDIO alignment adds - `skb_pull(skb, msdu_offset + 2); //+2 since sdio allign 58->60`. The
/// packet's own first word is the header's first: there is no separate bus header on a data packet, and
/// the 16-bit length at its start counts the frame AFTER these 60 bytes.
pub const RX_DATA_HDR: usize = 60;

/// What `hw_rxhdr` says about the frame behind it (`rwnx_rx.h`; bitfields from the least significant bit).
pub struct RxData {
    /// The 802.11 frame's length, from the packet's first 16 bits.
    pub len: usize,
    /// `flags_upload`: the vendor driver hands a frame up only when this is set.
    pub upload: bool,
    /// `flags_is_80211_mpdu`: a management frame, which goes to `rwnx_rx_mgmt_any`, not the data path.
    pub mpdu: bool,
    pub vif: u8,
    pub sta: u8,
    /// `decr_status` is `RWNX_RX_HD_DECR_CCMP128`: the firmware decrypted it and LEFT the 8-byte CCMP
    /// header in place, so the LLC/SNAP header starts 8 bytes later.
    pub ccmp: bool,
}

pub fn rx_header(pkt: &[u8]) -> Option<RxData> {
    if pkt.len() < RX_DATA_HDR {
        return None;
    }
    let f = pkt[48];
    Some(RxData {
        len: u16::from_le_bytes([pkt[0], pkt[1]]) as usize,
        upload: f & 0x40 != 0,
        mpdu: f & 0x02 != 0,
        vif: pkt[49],
        sta: pkt[50],
        ccmp: (pkt[36] >> 2) & 0x7 == 3,
    })
}

/// A received data frame, as the vendor driver turns it into an ethernet frame (`rwnx_rxdataind_aicwf`):
/// DA is address 1, SA is address 3 on a frame from the access point (address 2 otherwise).
pub struct DataIn<'a> {
    pub ethertype: u16,
    pub da: [u8; 6],
    pub sa: [u8; 6],
    pub body: &'a [u8],
}

/// The ethertype and body of a received 802.11 data frame: the MAC header (24 bytes, 26 with QoS, 4 more
/// when the order bit is set - the vendor's own test), the 8-byte CCMP header when `ccmp` (the firmware
/// decrypts but leaves it; the frame's protected bit is not what decides it, `decr_status` is), then
/// LLC/SNAP `aa aa 03 00 00 00` and the ethertype. `None` for anything that is not a data frame carrying
/// LLC/SNAP. Whether a CCMP frame's length counts its 8-byte MIC is not in the source (the line that would
/// strip it is commented out): a body may carry 8 trailing bytes, which EAPOL and IP both bound by their
/// own length fields.
pub fn llc_payload(frame: &[u8], ccmp: bool) -> Option<DataIn<'_>> {
    if frame.len() < 24 || frame[0] & 0x0c != 0x08 {
        return None;
    }
    let qos = frame[0] & 0x80 != 0;
    let order = frame[1] & 0x80 != 0;
    let from_ds = frame[1] & 0x03 == 0x02;
    let mut at = 24 + if qos { 2 } else { 0 } + if order { 4 } else { 0 };
    if ccmp {
        at += 8;
    }
    if frame.len() < at + 8 || frame[at..at + 6] != [0xaa, 0xaa, 0x03, 0x00, 0x00, 0x00] {
        return None;
    }
    let mut da = [0u8; 6];
    da.copy_from_slice(&frame[4..10]);
    let mut sa = [0u8; 6];
    sa.copy_from_slice(if from_ds { &frame[16..22] } else { &frame[10..16] });
    Some(DataIn { ethertype: u16::from_be_bytes([frame[at + 6], frame[at + 7]]), da, sa, body: &frame[at + 8..] })
}

/// `d` as an ethernet frame into `out`: DA, SA, ethertype, body. 0 when `out` cannot hold it.
pub fn to_ethernet(d: &DataIn, out: &mut [u8]) -> usize {
    let n = 14 + d.body.len();
    if n > out.len() {
        return 0;
    }
    out[0..6].copy_from_slice(&d.da);
    out[6..12].copy_from_slice(&d.sa);
    out[12..14].copy_from_slice(&d.ethertype.to_be_bytes());
    out[14..n].copy_from_slice(d.body);
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The header the chip ACCEPTED on the board for the memory read (`10 00 11 d5`, 2026-10-04) and for
    /// the start (`14 00 11 7e`): the CRC is right because the chip answered both.
    #[test]
    fn header_crc_matches_what_the_chip_accepted() {
        assert_eq!(crc8(&[0x10, 0x00, 0x11]), 0xd5);
        assert_eq!(crc8(&[0x14, 0x00, 0x11]), 0x7e);
    }

    #[test]
    fn a_memory_read_is_one_block_with_its_fields_in_place() {
        let mut f = [0u8; FRAME_MAX];
        assert_eq!(build_frame(0x0400, 1, &0x4050_0000u32.to_le_bytes(), &mut f), Some(512));
        assert_eq!(&f[..4], &[0x10, 0x00, 0x11, 0xd5]);
        assert_eq!(&f[4..8], &[0, 0, 0, 0]);
        assert_eq!(&f[8..20], &[0x00, 0x04, 0x01, 0x00, 0x64, 0x00, 0x04, 0x00, 0x00, 0x00, 0x50, 0x40]);
        assert!(f[20..].iter().all(|&b| b == 0));
    }

    /// 1032 parameter bytes: header length 1044 (`0x414`), 1052 with the tail, three blocks.
    #[test]
    fn a_block_write_is_three_blocks() {
        let mut f = [0u8; FRAME_MAX];
        assert_eq!(build_frame(0x040B, 1, &[0xAA; 1032], &mut f), Some(1536));
        assert_eq!((f[0], f[1]), (0x14, 0x04));
        assert_eq!(&f[14..16], &[0x08, 0x04]); // param_len 1032
        assert_eq!(f[16 + 1031], 0xAA);
        assert_eq!(f[16 + 1032], 0);
    }

    /// A frame that ends exactly on a block gets no tail; one that would overrun the largest frame is refused.
    #[test]
    fn padding_and_the_limit() {
        let mut f = [0u8; FRAME_MAX];
        // 4 + 4 + 8 + 496 = 512 exactly.
        assert_eq!(build_frame(1, 0, &[0; 496], &mut f), Some(512));
        // One more word crosses into the next block.
        assert_eq!(build_frame(1, 0, &[0; 500], &mut f), Some(1024));
        assert_eq!(build_frame(1, 0, &[0; 1600], &mut f), None);
    }

    /// The vendored table, walked, gives what the board's upload used (2026-10-04): seven groups in this
    /// order, the information group's addresses, 127 writes outside the version group.
    #[test]
    fn the_vendored_patch_table() {
        let t = include_bytes!("../../../nonfree/aic8800d80/fw_patch_table_8800d80_u02.bin");
        let Some(groups) = Groups::new(t) else {
            assert!(false, "the vendored table lost its tag");
            return;
        };
        let g: Vec<_> = groups.collect();
        let names: Vec<_> = g.iter().map(|(n, ty, p)| (core::str::from_utf8(n).unwrap_or("?"), *ty, p.len() / 8)).collect();
        assert_eq!(names, vec![
            ("AICBT_PINF_T", 0, 6), ("AICBT_TRAP_T", 1, 27), ("AICBT_PATCH_TB4", 2, 52), ("AICBT_MODE_T", 3, 9),
            ("AICBT_POWER_ON", 4, 3), ("AICBT_PATCH_TAF", 5, 30), ("AICBT_VER_INFO", 6, 8),
        ]);
        let inf = g[0].2;
        assert_eq!((le32(inf, 4), le32(inf, 12), le32(inf, 44)), (0x0020_1940, 0x001e_0000, 0x0020_b43c));
        assert_eq!(g.iter().filter(|(_, ty, _)| *ty != 6).map(|(_, _, p)| p.len() / 8).sum::<usize>(), 127);
        assert!(Groups::new(b"NOT_A_TABLE_AT_ALL").is_none());
    }

    #[test]
    fn rf_parameter_blocks() {
        let t = txpwr_lvl_v3();
        assert_eq!(t[0], 1);
        assert_eq!(t[1], 20);
        assert_eq!(&t[35..39], &[0x80; 4]); // the 5 GHz 11a row's unused rates
        assert_eq!(t[68], 14);
        assert!(t[69..].iter().all(|&b| b == 0));
        let r = rf_calib();
        assert_eq!(le32(&r, 0), 0x0f8f);
        assert_eq!(le32(&r, 8), 0x0c34_c008);
        assert_eq!(le32(&r, 16), 0x0026_4203);
        assert_eq!(&r[20..], &[0; 4]);
    }

    /// ME_CONFIG's fields where `struct me_config_req` puts them (natural alignment, no packing), so a
    /// byte moved in the table above fails here rather than on the chip.
    #[test]
    fn me_config_fields_sit_at_their_offsets() {
        let p = &ME_CONFIG;
        assert_eq!(u16::from_le_bytes([p[0], p[1]]), 0x0963); // HT capability
        assert_eq!(p[2], 0x1f); // A-MPDU parameters
        assert_eq!(p[3], 0xff); // MCS 0-7
        assert_eq!(p[7], 0x01); // MCS32
        assert_eq!(u16::from_le_bytes([p[13], p[14]]), 150); // HT highest rate
        assert_eq!(p[15], 0x01); // TX MCS set defined
        assert_eq!(le32(p, 32), 0x0398_7131); // VHT capability
        assert_eq!(u16::from_le_bytes([p[36], p[37]]), 0xfffe);
        assert_eq!(u16::from_le_bytes([p[38], p[39]]), 390);
        assert_eq!(&p[44..50], &[0, 0, 2, 0, 0, 0]); // HE MAC
        assert_eq!(&p[50..61], &[0x06, 0xe0, 0x2b, 0x58, 0x0d, 0xc0, 0xcf, 0x00, 0x02, 0x30, 0x00]); // HE PHY
        assert_eq!(u16::from_le_bytes([p[62], p[63]]), 0xfffe); // HE MCS rx 80
        assert_eq!(u16::from_le_bytes([p[64], p[65]]), 0xfffe); // HE MCS tx 80
        assert_eq!(&p[74..78], &[0x38, 0x1c, 0xc7, 0x01]); // PPE thresholds
        assert_eq!(u16::from_le_bytes([p[100], p[101]]), 1000); // tx lifetime
        assert_eq!(&p[102..110], &[2, 1, 1, 1, 0, 1, 1, 0]);
    }

    #[test]
    fn channel_list() {
        let p = chan_config(true);
        assert_eq!((p[252], p[253]), (14, 25));
        assert_eq!(&p[0..5], &[0x6c, 0x09, 0, 0, 20]); // 2412 MHz, channel 1
        assert_eq!(u16::from_le_bytes([p[13 * 6], p[13 * 6 + 1]]), 2484); // channel 14
        assert_eq!(u16::from_le_bytes([p[84], p[85]]), 5180);
        assert_eq!(p[86], 1); // 5 GHz band
        assert_eq!(u16::from_le_bytes([p[84 + 24 * 6], p[84 + 24 * 6 + 1]]), 5825); // the 25th
        assert!(p[84 + 25 * 6..252].iter().all(|&b| b == 0)); // three unused slots
        let q = chan_config(false);
        assert_eq!((q[252], q[253]), (14, 0));
        assert!(q[84..252].iter().all(|&b| b == 0));
    }

    #[test]
    fn start_coex_and_interface() {
        let s = mm_start();
        assert!(s[..64].iter().all(|&b| b == 0));
        assert_eq!(&s[64..72], &[0x2c, 0x01, 0, 0, 0x14, 0, 0, 0]);
        assert_eq!(COEX[0], 1);
        assert_eq!(COEX[2], 1);
        assert_eq!(add_if([1, 2, 3, 4, 5, 6]), [0, 0, 1, 2, 3, 4, 5, 6, 0, 0]);
    }

    /// The layout the vendor's struct gives (`lmac_msg.h`, natural alignment): channels at 0, SSIDs at 252,
    /// the BSSID at 352, the index and counts at 366-368.
    #[test]
    fn scan_request() {
        let p = scanu_start(0, true);
        assert_eq!(&p[0..6], &[0x6c, 0x09, 0, 0, 20, 0]); // 2412
        assert_eq!(&p[84..90], &[0x3c, 0x14, 1, 0, 20, 0]); // 5180
        assert!(p[234..352].iter().all(|&b| b == 0)); // unused channels, an empty SSID
        assert_eq!(&p[352..358], &[0xff; 6]);
        assert_eq!((p[366], p[367], p[368], p[369]), (0, 39, 1, 0));
        assert!(p[370..].iter().all(|&b| b == 0));
        let q = scanu_start(2, false);
        assert_eq!((q[366], q[367]), (2, 14));
    }

    /// A made-up beacon in the indication's layout: header fields, then a 24-byte MAC header with the
    /// BSSID at 16, timestamp, interval, capability (privacy), an SSID element and an RSN element.
    #[test]
    fn a_result_indication() {
        let mut frame = [0u8; 36 + 2 + 4 + 2 + 2];
        frame[0] = 0x80; // beacon
        frame[16..22].copy_from_slice(&[0x02, 0x11, 0x22, 0x33, 0x44, 0x55]);
        frame[34] = 0x11; // capability: ESS | privacy
        frame[36..42].copy_from_slice(&[0, 4, b't', b'e', b's', b't']);
        frame[42..44].copy_from_slice(&[48, 2]);
        let mut params = [0u8; 12 + 46];
        params[0..2].copy_from_slice(&(frame.len() as u16).to_le_bytes());
        params[4..6].copy_from_slice(&2437u16.to_le_bytes());
        params[6] = 0;
        params[9] = (-52i8) as u8;
        params[12..].copy_from_slice(&frame);
        let Some(r) = parse_result(&params) else {
            assert!(false, "a well-formed indication was refused");
            return;
        };
        assert_eq!((r.freq, r.band, r.rssi), (2437, 0, -52));
        assert_eq!(r.bssid(), [0x02, 0x11, 0x22, 0x33, 0x44, 0x55]);
        assert_eq!(r.capability(), 0x11);
        assert_eq!(r.ssid(), Some(&b"test"[..]));
        assert_eq!(channel_of(r.freq), 6);
        // A length that runs past the parameters is refused, not read past.
        params[0..2].copy_from_slice(&200u16.to_le_bytes());
        assert!(parse_result(&params).is_none());
    }

    #[test]
    fn channel_numbers() {
        assert_eq!(channel_of(2412), 1);
        assert_eq!(channel_of(2472), 13);
        assert_eq!(channel_of(2484), 14);
        assert_eq!(channel_of(5180), 36);
        assert_eq!(channel_of(5825), 165);
        assert_eq!(channel_of(1000), 0);
    }

    #[test]
    fn connect_request() {
        let rsn = [0x30, 0x14, 1, 0, 0, 0x0f, 0xac, 4, 1, 0, 0, 0x0f, 0xac, 4, 1, 0, 0, 0x0f, 0xac, 2, 0, 0];
        let Some(p) = sm_connect(b"net", [1, 2, 3, 4, 5, 6], 2437, 0, 0, &rsn) else {
            assert!(false, "a valid connect was refused");
            return;
        };
        assert_eq!(&p[0..4], &[3, b'n', b'e', b't']);
        assert_eq!(&p[34..40], &[1, 2, 3, 4, 5, 6]);
        assert_eq!(&p[40..43], &[0x85, 0x09, 0]);
        assert_eq!(p[44], 0); // transmit power left 0
        assert_eq!(le32(&p, 48), 0x09);
        assert_eq!(&p[52..54], &[0x88, 0x8e]);
        assert_eq!(u16::from_le_bytes([p[54], p[55]]), 22);
        assert_eq!((p[59], p[60], p[61]), (0, 1, 0));
        assert_eq!(&p[64..86], &rsn);
        assert!(sm_connect(&[0; 33], [0; 6], 0, 0, 0, &[]).is_none());
        let Some(o) = sm_connect(b"open", [0xff; 6], 0xffff, 0, 0, &[]) else {
            assert!(false, "an open connect was refused");
            return;
        };
        assert_eq!(le32(&o, 48), 0); // no control port, no WPA
        assert_eq!(u16::from_le_bytes([o[54], o[55]]), 0);
        assert_eq!(sta_info_rssi(&[0, 0, 0, 0, 0, 0, 0, 0, 0xc4]), Some(-60));
        assert_eq!(sta_info_rssi(&[0; 8]), None);
        assert!(sm_connect(b"x", [0; 6], 0, 0, 0, &[0; 257]).is_none());
    }

    #[test]
    fn connect_indication() {
        let mut p = [0u8; 852];
        p[2..8].copy_from_slice(&[9, 8, 7, 6, 5, 4]);
        p[10] = 3;
        p[820] = 7;
        p[824..826].copy_from_slice(&5180u16.to_le_bytes());
        let Some(c) = parse_connect_ind(&p) else {
            assert!(false, "a full indication was refused");
            return;
        };
        assert_eq!((c.status, c.ap_idx, c.aid, c.freq), (0, 3, 7, 5180));
        assert_eq!(c.bssid, [9, 8, 7, 6, 5, 4]);
        assert!(parse_connect_ind(&p[..100]).is_none());
    }

    #[test]
    fn keys_port_and_disconnect() {
        let k = [0x5a; 16];
        let pw = key_add(0, 3, &k, true, 0);
        assert_eq!((pw[0], pw[1], pw[4], pw[40], pw[43]), (0, 3, 16, 2, 1));
        assert_eq!(&pw[8..24], &k);
        let g = key_add(1, 0xff, &k, false, 0);
        assert_eq!((g[0], g[1], g[43]), (1, 0xff, 0));
        assert_eq!(control_port(3, true), [3, 1]);
        assert_eq!(disconnect(3, 0), [3, 0, 0, 0]);
    }

    /// An EAPOL frame out: the data type byte, the descriptor's fields in place, the body after it.
    #[test]
    fn a_data_frame() {
        let mut f = [0u8; FRAME_MAX];
        let body = [2u8, 3, 0, 0x5f];
        assert_eq!(build_data_frame([1; 6], [2; 6], 0x888e, &body, 0, 3, 0x8000_0001, &mut f), Some(512));
        assert_eq!((f[0], f[1], f[2]), (32, 0, TYPE_DATA_TX));
        assert_eq!(f[3], crc8(&[32, 0, 1]));
        assert_eq!(u16::from_le_bytes([f[4], f[5]]), 4);
        assert_eq!(le32(&f, 8), 0x8000_0001);
        assert_eq!(&f[12..18], &[1; 6]);
        assert_eq!(&f[18..24], &[2; 6]);
        assert_eq!(&f[24..26], &[0x88, 0x8e]);
        assert_eq!((f[28], f[29]), (0, 3));
        assert_eq!(&f[32..36], &body);
    }

    /// An EAPOL frame in: a QoS data frame from the AP, LLC/SNAP, ethertype 888e, turned into ethernet
    /// with DA = address 1 and SA = address 3. And a frame the firmware decrypted, with the 8-byte CCMP
    /// header it leaves in place - decided by the header's `decr_status`, not the protected bit.
    #[test]
    fn eapol_in_a_data_frame() {
        let mut f = [0u8; 26 + 8 + 4];
        f[0] = 0x88; // QoS data
        f[1] = 0x02; // from DS
        f[4..10].copy_from_slice(&[1; 6]);
        f[10..16].copy_from_slice(&[7; 6]);
        f[16..22].copy_from_slice(&[9; 6]);
        f[26..34].copy_from_slice(&[0xaa, 0xaa, 3, 0, 0, 0, 0x88, 0x8e]);
        f[34..38].copy_from_slice(&[2, 3, 0, 0x5f]);
        let Some(d) = llc_payload(&f, false) else {
            assert!(false, "an EAPOL data frame was not recognised");
            return;
        };
        assert_eq!((d.ethertype, d.da, d.sa), (0x888e, [1; 6], [9; 6]));
        assert_eq!(d.body, &[2, 3, 0, 0x5f]);
        let mut eth = [0u8; 32];
        assert_eq!(to_ethernet(&d, &mut eth), 18);
        assert_eq!(&eth[..14], &[1, 1, 1, 1, 1, 1, 9, 9, 9, 9, 9, 9, 0x88, 0x8e]);
        assert_eq!(to_ethernet(&d, &mut [0u8; 17]), 0);
        let mut g = [0u8; 26 + 8 + 8 + 4];
        g[0] = 0x88;
        g[1] = 0x42; // from DS, protected
        g[34..42].copy_from_slice(&[0xaa, 0xaa, 3, 0, 0, 0, 0x88, 0x8e]);
        assert_eq!(llc_payload(&g, true).map(|d| (d.ethertype, d.body.len())), Some((0x888e, 4)));
        assert!(llc_payload(&g, false).is_none()); // the IV read as LLC/SNAP is not LLC/SNAP
        assert!(llc_payload(&[0x80; 40], false).is_none()); // a beacon is not data
    }

    /// The receive header's fields where `rwnx_rx.h` puts them.
    #[test]
    fn a_receive_header() {
        let mut p = [0u8; RX_DATA_HDR];
        p[0..2].copy_from_slice(&120u16.to_le_bytes());
        p[36] = 3 << 2; // decr_status CCMP128
        p[48] = 0x40; // upload
        p[49] = 0;
        p[50] = 4;
        let Some(h) = rx_header(&p) else {
            assert!(false, "a whole header was refused");
            return;
        };
        assert_eq!((h.len, h.upload, h.mpdu, h.vif, h.sta, h.ccmp), (120, true, false, 0, 4, true));
        assert!(rx_header(&p[..59]).is_none());
    }
}
