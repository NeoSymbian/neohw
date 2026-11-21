// SPDX-License-Identifier: Apache-2.0
//! Wi-Fi's user side (docs hardware/wifi.md, hardware.md S5), driven by WIFID.EXE:
//! - [`Firmware`]: the WCN3990 firmware's start (WLFW, QMI 69: MSA, board data, mission mode) over a NeoHub
//!   [`Bus`]; HWHUB only routes its packets (Wi-Fi W1/W2);
//! - [`ipc`]: the NeoWifi contract (WIFID <-> neoui's Settings): scan, link, join, leave, the radio switch;
//! - [`kernel`]: the kernel's scan and link records (HAL WlanScan 0x4e1e, WlanLink 0x4e1f), and the saved networks.
#![no_std]
extern crate alloc;

pub mod ipc;
pub mod kernel;

use alloc::vec::Vec;
use hub_ipc::{Bus, Event};
pub use wlfw;

/// our WLFW client port
pub const PORT: u32 = 0x26;
const MODEM_NODE: u32 = 0;
/// DTB wlan-msa@94d00000: the MSA carve-out the Wi-Fi firmware may use
const WLAN_MSA: (u64, u32) = (0x94d0_0000, 0x10_0000);

/// What the firmware's start needs from the device (kernel HALs and the vendor partition).
pub trait Hal {
    /// Hand the firmware's MSA regions to its VMIDs (TrustZone; HAL WlanMsa). W1.
    fn wlan_msa(&mut self, regions: &[wlfw::Region]) -> Result<(), i32>;
    /// The board data file for `board_id` (vendor_a), or empty if there is none.
    fn board_data(&mut self, board_id: u32) -> Vec<u8>;
    /// The firmware is in mission mode: start the host's copy engines, HTC and WMI (HAL WlanStart). W2.
    fn wlan_start(&mut self) -> Result<(), i32>;
}

const START_RETRIES: u8 = 30;
const KERR_NOT_READY: i32 = -18;
const KERR_IN_USE: i32 = -14;

/// The firmware's control client, once its service appears on the modem's node.
pub struct Firmware {
    client: Option<((u32, u32), wlfw::Client)>,
    /// the board id the firmware reported (kept across restarts: the board file to load)
    board: u32,
    /// mission mode reached (the kernel's copy engines run)
    pub enabled: bool,
    /// mission mode reached but the kernel refused WlanStart (it does while a modem stop finishes): tries left,
    /// one per `retry_start` (WIFID calls it every second)
    start_retries: u8,
}

impl Default for Firmware {
    fn default() -> Self {
        Self::new()
    }
}

impl Firmware {
    pub fn new() -> Self {
        Firmware {
            client: None,
            board: 0xff,
            enabled: false,
            start_retries: 0,
        }
    }

    /// The kernel's copy engines for the firmware in mission mode (WlanStart); refused while a modem stop is still
    /// finishing (KErrNotReady) or the driver lets go (KErrInUse): tried again by `retry_start`.
    fn start(&mut self, bus: &mut dyn Bus, hal: &mut dyn Hal) {
        let r = hal.wlan_start();
        self.enabled = r.is_ok();
        let again = matches!(r, Err(KERR_NOT_READY | KERR_IN_USE)) && self.start_retries > 1;
        if r.is_ok() || !again || self.start_retries == START_RETRIES {
            bus.log(format_args!(
                "wlfw: mission mode; host copy engines start -> {r:?}{}",
                if again {
                    " (trying again every second)"
                } else {
                    ""
                }
            ));
        }
        self.start_retries = if again { self.start_retries - 1 } else { 0 };
    }

    /// Once a second: a refused WlanStart again (up to 30 s).
    pub fn retry_start(&mut self, bus: &mut dyn Bus, hal: &mut dyn Hal) {
        if self.start_retries > 0 && !self.enabled {
            self.start(bus, hal);
        }
    }

    pub fn bind(&self, bus: &mut dyn Bus) -> Result<(), i32> {
        bus.bind(wlfw::SERVICE, PORT)
    }

    pub fn event(&mut self, bus: &mut dyn Bus, hal: &mut dyn Hal, e: &Event) {
        match e {
            Event::ServerUp(s) if s.service == wlfw::SERVICE && s.node == MODEM_NODE => {
                let mut c = wlfw::Client::new(WLAN_MSA.0, WLAN_MSA.1);
                let steps = c.start();
                self.client = Some(((s.node, s.port), c));
                self.enabled = false;
                bus.log(format_args!(
                    "wlfw: the Wi-Fi firmware's service is up; starting"
                ));
                self.steps(bus, hal, steps);
            }
            Event::ServerDown(s)
                if self
                    .client
                    .as_ref()
                    .is_some_and(|c| c.0 == (s.node, s.port)) =>
            {
                bus.log(format_args!("wlfw: the Wi-Fi firmware's service went"));
                self.client = None;
                self.enabled = false;
                self.start_retries = 0;
            }
            Event::Packet {
                src,
                dst_port: PORT,
                payload,
            } => {
                let steps = match &mut self.client {
                    Some((dst, c)) if dst == src => c.message(payload),
                    _ => return,
                };
                self.steps(bus, hal, steps);
            }
            // the modem's stop takes the WLAN domain with it (W7: the kernel restarts the copy engines later)
            Event::Reset { .. } => {
                self.client = None;
                self.enabled = false;
                self.start_retries = 0;
            }
            _ => {}
        }
    }

    /// Carry out what the WLFW client asks for: sends, the MSA assignment, the board data.
    fn steps(&mut self, bus: &mut dyn Bus, hal: &mut dyn Hal, mut steps: Vec<wlfw::Step>) {
        while !steps.is_empty() {
            let mut next = Vec::new();
            for s in steps {
                let Some((dst, _)) = self.client else { return };
                match s {
                    wlfw::Step::Send(m) => {
                        let id = qrtr::qmi::Msg::parse(&m).map_or(0, |q| q.id);
                        bus.log(format_args!("wlfw: request {id:#x}"));
                        bus.send(PORT, dst, &m, true);
                    }
                    wlfw::Step::AssignMsa(regions) => {
                        for r in &regions {
                            bus.log(format_args!(
                                "wlfw: MSA region {:#x} + {:#x}{}",
                                r.addr,
                                r.size,
                                if r.secure { " (secure)" } else { "" }
                            ));
                        }
                        let r = hal.wlan_msa(&regions);
                        bus.log(format_args!("wlfw: MSA assigned -> {r:?}"));
                        if let Some((_, c)) = &mut self.client {
                            next.extend(c.msa_assigned(r.is_ok()));
                        }
                    }
                    wlfw::Step::LoadBoardData => {
                        let data = hal.board_data(self.board);
                        bus.log(format_args!("wlfw: board data {} bytes", data.len()));
                        if let Some((_, c)) = &mut self.client {
                            next.extend(c.board_data(data));
                        }
                    }
                    wlfw::Step::Caps(k) => {
                        self.board = k.board_id;
                        bus.log(format_args!(
                            "wlfw: chip {:#x} family {:#x} board {:#x} soc {:#x} fw {:#x} {} {}",
                            k.chip_id,
                            k.chip_family,
                            k.board_id,
                            k.soc_id,
                            k.fw_version,
                            k.fw_timestamp,
                            k.fw_build_id
                        ));
                    }
                    wlfw::Step::Ready => bus.log(format_args!("wlfw: firmware ready (W1 done)")),
                    wlfw::Step::Enabled => {
                        self.start_retries = START_RETRIES;
                        self.start(bus, hal);
                    }
                    wlfw::Step::Failed(what, e) => {
                        bus.log(format_args!("wlfw: {what} failed ({e})"))
                    }
                }
            }
            steps = next;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use core::fmt;
    use hub_ipc::Server;

    /// (our port, destination, payload, essential)
    type Sent = (u32, (u32, u32), Vec<u8>, bool);

    #[derive(Default)]
    struct Fake {
        binds: Vec<(u32, u32)>,
        sent: Vec<Sent>,
        msa: usize,
        /// WlanStart answers, first one first (empty: Ok)
        starts: Vec<i32>,
    }
    impl Bus for Fake {
        fn bind(&mut self, service: u32, port: u32) -> Result<(), i32> {
            self.binds.push((service, port));
            Ok(())
        }
        fn send(&mut self, port: u32, dst: (u32, u32), payload: &[u8], essential: bool) -> bool {
            self.sent.push((port, dst, payload.to_vec(), essential));
            true
        }
        fn log(&mut self, _: fmt::Arguments) {}
        fn utc_ms(&mut self) -> i64 {
            0
        }
    }
    impl Hal for Fake {
        fn wlan_msa(&mut self, _: &[wlfw::Region]) -> Result<(), i32> {
            self.msa += 1;
            Ok(())
        }
        fn board_data(&mut self, _: u32) -> Vec<u8> {
            vec![]
        }
        fn wlan_start(&mut self) -> Result<(), i32> {
            match self.starts.is_empty() {
                true => Ok(()),
                false => Err(self.starts.remove(0)),
            }
        }
    }

    #[test]
    fn a_refused_wlan_start_is_tried_again_until_the_kernel_takes_it() {
        let (mut fw, mut f, mut h) = (Firmware::new(), Fake::default(), Fake::default());
        h.starts = vec![KERR_NOT_READY, KERR_IN_USE]; // a modem stop still finishing
        fw.start_retries = START_RETRIES;
        fw.start(&mut f, &mut h);
        assert!(!fw.enabled);
        fw.retry_start(&mut f, &mut h);
        assert!(!fw.enabled);
        fw.retry_start(&mut f, &mut h);
        assert!(fw.enabled);
        // any other error is final
        let (mut fw, mut h) = (
            Firmware::new(),
            Fake {
                starts: vec![-5],
                ..Fake::default()
            },
        );
        fw.start_retries = START_RETRIES;
        fw.start(&mut f, &mut h);
        fw.retry_start(&mut f, &mut h);
        assert!(!fw.enabled && h.starts.is_empty());
    }

    fn up(node: u32, port: u32) -> Event {
        Event::ServerUp(Server {
            service: wlfw::SERVICE,
            instance: 1,
            node,
            port,
        })
    }

    #[test]
    fn binds_and_starts_only_for_the_modems_service() {
        let (mut fw, mut f, mut h) = (Firmware::new(), Fake::default(), Fake::default());
        fw.bind(&mut f).unwrap();
        assert_eq!(f.binds, [(wlfw::SERVICE, PORT)]);
        fw.event(&mut f, &mut h, &up(5, 9)); // the ADSP's node: not the Wi-Fi firmware
        assert!(f.sent.is_empty());
        fw.event(&mut f, &mut h, &up(MODEM_NODE, 83));
        let (port, dst, _, essential) = &f.sent[0];
        assert_eq!((*port, *dst, *essential), (PORT, (MODEM_NODE, 83), true));
    }

    #[test]
    fn packets_from_elsewhere_and_after_a_reset_are_ignored() {
        let (mut fw, mut f, mut h) = (Firmware::new(), Fake::default(), Fake::default());
        fw.event(&mut f, &mut h, &up(MODEM_NODE, 83));
        let n = f.sent.len();
        let stray = Event::Packet {
            src: (MODEM_NODE, 84),
            dst_port: PORT,
            payload: vec![0; 8],
        };
        fw.event(&mut f, &mut h, &stray);
        assert_eq!(f.sent.len(), n);
        fw.event(
            &mut f,
            &mut h,
            &Event::Reset {
                subsystem: hub_ipc::Subsystem::Mss,
                generation: 2,
            },
        );
        assert!(fw.client.is_none() && !fw.enabled);
    }
}
