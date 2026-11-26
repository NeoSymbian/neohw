// SPDX-License-Identifier: Apache-2.0
//! HWHUB.EXE: sole owner of the MSS and ADSP QRTR rings and C5/C6/C7 hardware lifecycle.
//! MODEMD keeps APR ring 2 until S4; NeoHub carries the payload boundary.
#![no_std]
extern crate alloc;
mod adsp;
use alloc::{vec, vec::Vec};
use core::{fmt, ops::ControlFlow, pin::pin};
use e32::rt::{self, Either};
use e32::server::{self, Message, Server, Service};
use e32::sys::{err, ipc::TVersion};
use e32::{dprintln, time};
use hub::recovery::{self, StopProgress};
use hub::{Platform, Router, ssctl};
use hub_ipc::{Bus, Event, SessionQueue, Snapshot, op};
use sys::{
    BOOT, IPA_START, Io, PART_READ, RECV, RMTFS_MEM, RPROC_STATE, RPROC_STOP, SEND, STATE,
    TRY_SEND, ext4_err, fs_size, fw_read, hal, hal_io, ipa_firmware, vendor_file,
};
const NOT_READY: i32 = -18;
/// a modem restarted this recently restarts again only after a crash (requests are logged and ignored)
const RESTART_HOLD_MS: u64 = 30_000;
/// how long a running modem gets to say it is ready to shut down (Linux: 5 s answer + 10 s ack)
const SHUTDOWN_WAIT_MS: u64 = 10_000;
/// after 3 failed attempts, the next try on its own
const RETRY_FAILED_MS: u64 = 300_000;
const PART_SIZES: [usize; 4] = [0x20_0000, 0x20_0000, 0x20_0000, 0x2_0000];
const RUNNING: u32 = 8;
const MODEM_NODE: u32 = 0;
const FATAL: u32 = 1;
/// slave-kernel bit 7: the modem acknowledged an SSCTL shutdown (Linux sysmon's shutdown-ack interrupt)
const SHUTDOWN_ACK: u32 = 1 << 7;
const BOOT_TIMEOUT_MS: u64 = 60_000;
const LOOP_MS: u64 = 10;
const STATUS_LOG_MS: u64 = 30_000;
/// boot step, error, SMP2P slave-kernel bits, IPCRTR open
fn state() -> Result<[u32; 4], i32> {
    let mut s = [0u32; 4];
    // SAFETY: STATE writes the live four-word array and takes no second argument.
    let r = unsafe { hal(STATE, s.as_mut_ptr().cast(), core::ptr::null_mut()) };
    if r < 0 { Err(r) } else { Ok(s) }
}

fn processor_stop_state() -> Result<[u32; 4], i32> {
    let mut s = [0u32; 4];
    // SAFETY: RPROC_STATE writes the live four-word array and takes no second argument.
    let r = unsafe { hal(RPROC_STATE, s.as_mut_ptr().cast(), core::ptr::null_mut()) };
    if r < 0 { Err(r) } else { Ok(s) }
}

struct Hal;

impl Platform for Hal {
    fn send(&mut self, node: u32, packet: &[u8]) -> Result<(), i32> {
        if node != MODEM_NODE {
            return adsp::send(adsp::RING_QRTR, packet); // the ADSP's router (C6)
        }
        // No userspace retry sleeps: mandatory packets use the ordered outbox. The synchronous kernel HAL
        // may still wait for a receive intent; this is not a hard real-time send deadline.
        // SAFETY: SEND reads packet.len() bytes from the live packet slice.
        match unsafe { hal(SEND, packet.as_ptr() as *mut u8, packet.len() as *mut u8) } {
            r if r < 0 => Err(r),
            _ => Ok(()),
        }
    }

    fn try_send(&mut self, node: u32, packet: &[u8]) -> Result<(), i32> {
        if node != MODEM_NODE {
            return self.send(node, packet);
        }
        // SAFETY: TRY_SEND reads packet.len() bytes from the live packet slice, synchronously.
        match unsafe {
            hal(
                TRY_SEND,
                packet.as_ptr() as *mut u8,
                packet.len() as *mut u8,
            )
        } {
            err::KERR_NOT_SUPPORTED => self.send(node, packet), // an older kernel
            r if r < 0 => Err(r),
            _ => Ok(()),
        }
    }

    fn rmtfs_read(&mut self, offset: u32, buf: &mut [u8]) -> Result<(), i32> {
        let io = Io {
            a: 1,
            b: offset,
            len: buf.len() as u32,
            buf: buf.as_mut_ptr() as u32,
        };
        // SAFETY: RMTFS_MEM receives the live slice address and its exact length for this synchronous copy.
        match unsafe { hal_io(RMTFS_MEM, io) }.0 {
            r if r < 0 => Err(r),
            _ => Ok(()),
        }
    }

    fn rmtfs_write(&mut self, offset: u32, data: &[u8]) -> Result<(), i32> {
        let io = Io {
            a: 2,
            b: offset,
            len: data.len() as u32,
            buf: data.as_ptr() as u32,
        };
        // SAFETY: RMTFS_MEM receives the live slice address and its exact length for this synchronous copy.
        match unsafe { hal_io(RMTFS_MEM, io) }.0 {
            r if r < 0 => Err(r),
            _ => Ok(()),
        }
    }

    fn log(&mut self, args: fmt::Arguments) {
        dprintln!("[hwhub] t={}ms {}", time::millis(), args);
    }

    fn utc_ms(&mut self) -> i64 {
        // before the network or the RTC set the clock it reads 1970-ish: unknown (the log skips pruning)
        match time::utc_now().0 {
            u if u < 1_600_000_000 => 0,
            u => u * 1000,
        }
    }

    fn fw_size(&mut self, path: &str) -> Result<usize, i32> {
        match path.strip_prefix("vendor/") {
            // SAFETY: Only the daemon's one loop thread accesses the vendor cache.
            Some(p) => unsafe { vendor_file(p, |fs, f| Ok(fs_size(fs, f))) },
            None => fw_read(path, 0, &mut []),
        }
    }

    fn fw_read(&mut self, path: &str, offset: usize, buf: &mut [u8]) -> Result<usize, i32> {
        match path.strip_prefix("vendor/") {
            // SAFETY: Only the daemon's one loop thread accesses the vendor cache.
            Some(p) => unsafe {
                vendor_file(p, |fs, f| {
                    fs.read_at(f, offset as u64, buf).map_err(ext4_err)
                })
            },
            None => fw_read(path, offset, buf),
        }
    }
}

enum Phase {
    /// IPA's GSI firmware from vendor_a, then IPA powered and the firmware loaded (C5b)
    Ipa,
    /// reading partition i (one per loop step)
    Read(usize),
    /// the ADSP loads and comes up before the modem boots (C6): loading it during the modem's start-up lost the
    /// modem's announcements, and the modem hears "adsp up" (SSCTL) as soon as both run
    Adsp,
    /// the kernel boots the modem; since t0
    Wait(u64),
    Running,
    /// a running modem was asked to shut down over SSCTL (Linux sysmon); the stop follows its ready
    /// indication or this deadline
    Shutdown {
        until: u64,
    },
    Recover {
        mask: u32,
        since: u64,
        issued: bool,
        /// A watchdog failure leaves stop ownership with the kernel until a terminal acknowledgement.
        watchdog: Option<i32>,
    },
    Failed,
    /// Startup radio-disabled build: ADSP services remain active.
    Disabled,
    /// No modem on this machine (QEMU): NeoHub stays available with an absent snapshot.
    Absent,
}

struct Client {
    id: u32,
    bindings: Vec<(u32, u32)>,
    queue: SessionQueue,
    receive: Option<Message>,
    quiesced: u32,
}
impl Client {
    fn push(&mut self, event: Event) {
        if self.queue.push(event) {
            dprintln!(
                "[hwhub] session {} dropped {} queued events",
                self.id,
                self.queue.dropped()
            );
        }
    }
}
struct Daemon {
    phase: Phase,
    images: [Vec<u8>; 4],
    router: Router,
    rx: Vec<u8>,
    adsp: adsp::Adsp,
    ipa_up: bool,
    t0: u64,
    last_log: u64,
    fatal_seen: bool,
    recovery_attempt: u8,
    /// the one stop that reclaims a load a previous HWHUB abandoned was issued
    adsp_reclaimed: bool,
    recovery_error: i32,
    healthy_since: u64,
    /// when the modem last reached Running (the restart window counts from here)
    running_since: u64,
    /// when HWHUB last gave up (Phase::Failed): it tries again on its own 5 minutes later
    failed_at: u64,
    quiesce_until: u64,
    clients: Vec<Client>,
    next_session: u32,
    published: Snapshot,
}
impl Daemon {
    /// One step of the work: Err(KErrNotSupported) when this machine has no modem (QEMU/Syborg).
    fn step(&mut self) -> Result<(), i32> {
        if matches!(self.phase, Phase::Absent) {
            return Ok(());
        }
        self.adsp.step();
        self.run();
        match self.phase {
            Phase::Ipa => {
                match ipa_firmware() {
                    Ok((image, mdt_len)) => {
                        // SAFETY: The image stays alive while IPA_START reads its declared length.
                        let (r, _) = unsafe {
                            hal_io(
                                IPA_START,
                                Io {
                                    a: mdt_len as u32,
                                    b: 0,
                                    len: image.len() as u32,
                                    buf: image.as_ptr() as u32,
                                },
                            )
                        };
                        dprintln!(
                            "[hwhub] ipa: firmware {} bytes (mdt {}), IPA bring-up -> {}",
                            image.len(),
                            mdt_len,
                            r
                        );
                        self.ipa_up = r == 0;
                    }
                    Err(ext4ro::Error::Io(e))
                        if e == err::KERR_NOT_SUPPORTED && !matches!(self.adsp, adsp::Adsp::Up) =>
                    {
                        dprintln!(
                            "[hwhub] no modem on this machine ({}): serving NeoHub without hardware",
                            e
                        );
                        self.phase = Phase::Absent;
                        return Ok(());
                    }
                    Err(e) => dprintln!("[hwhub] ipa: no firmware from vendor_a ({:?})", e),
                }
                self.phase = Phase::Read(0); // the modem boots either way
            }
            Phase::Read(i) => {
                let n = PART_SIZES[i];
                let mut v = vec![0u8; n];
                // SAFETY: PART_READ writes only the live buffer described by this Io.
                let (r, _) = unsafe {
                    hal_io(
                        PART_READ,
                        Io {
                            a: i as u32,
                            b: 0,
                            len: n as u32,
                            buf: v.as_mut_ptr() as u32,
                        },
                    )
                };
                if r == err::KERR_NOT_SUPPORTED && !matches!(self.adsp, adsp::Adsp::Up) {
                    return Err(r);
                }
                if r < 0 {
                    self.fail("partition read", r);
                    return Ok(());
                }
                // diagnosis (docs known-issues, the reason-4 drops): which EFS the modem starts from
                let fnv = v.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| {
                    (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)
                });
                dprintln!(
                    "[hwhub] EFS image {} FNV-1a {:016x}",
                    hub::rmtfs::PATHS[i],
                    fnv
                );
                self.images[i] = v;
                if i + 1 < PART_SIZES.len() {
                    self.phase = Phase::Read(i + 1);
                } else {
                    self.boot();
                }
            }
            // (the top of step() already stepped the ADSP and serviced its rings)
            Phase::Adsp => {
                if matches!(self.adsp, adsp::Adsp::Stale) {
                    // a previous HWHUB died mid-load: the processor stop releases the kernel's loader (PAS) and
                    // the ADSP's step, then the load starts again; a second refusal is a failure
                    self.adsp = adsp::Adsp::Failed;
                    if !self.adsp_reclaimed {
                        self.adsp_reclaimed = true;
                        dprintln!(
                            "[hwhub] adsp: an abandoned load holds the loader; stopping it first"
                        );
                        self.phase = Phase::Recover {
                            mask: 2,
                            since: time::millis(),
                            issued: false,
                            watchdog: None,
                        };
                        return Ok(());
                    }
                }
                if !self.adsp.busy() {
                    if matches!(self.adsp, adsp::Adsp::Failed) {
                        // no audio, but the radio still starts (PR #2)
                        dprintln!("[hwhub] ADSP unavailable; starting the radio without it");
                    }
                    if cfg!(feature = "radio-disabled") {
                        self.phase = Phase::Disabled;
                    } else if self.router.radio_attached() {
                        // a recovery restarted the ADSP: the radio storage is still attached, the modem boots again
                        self.boot_modem();
                    } else {
                        self.phase = Phase::Ipa;
                    }
                }
            }
            Phase::Wait(t0) => {
                let s = match state() {
                    Ok(s) => s,
                    Err(e) => {
                        self.fail("modem boot state", e);
                        return Ok(());
                    }
                };
                if s[1] != 0 {
                    self.fail("modem boot", s[1] as i32);
                } else if s[0] == RUNNING && s[3] != 0 {
                    let now = time::millis();
                    dprintln!("[hwhub] modem running after {} ms; QRTR up", now - t0);
                    self.t0 = now;
                    self.last_log = now;
                    self.router.start(&mut Hal);
                    self.phase = Phase::Running;
                    self.running_since = now;
                } else if time::millis() - t0 > BOOT_TIMEOUT_MS {
                    self.fail("modem boot (60 s)", NOT_READY);
                }
            }
            Phase::Recover {
                mask,
                since,
                issued,
                watchdog,
            } => {
                let now = time::millis();
                if now < since {
                    return Ok(());
                }
                if !issued {
                    if now < self.quiesce_until
                        && self.clients.iter().any(|c| {
                            !c.bindings.is_empty() && c.quiesced != self.router.generation()
                        })
                    {
                        return Ok(());
                    }
                    // A previous worker may still own shutdown, or its state may be unreadable. Neither
                    // permits another stop request. Do not interpret a HAL read failure as an idle worker.
                    if !matches!(processor_stop_state(), Ok([0 | 2 | 3, _, _, _])) {
                        if watchdog.is_none()
                            && now.saturating_sub(since) >= recovery::STOP_WATCHDOG_MS
                        {
                            dprintln!(
                                "[hwhub] t={}ms waiting for existing processor stop; quarantined",
                                now
                            );
                            self.phase = Phase::Recover {
                                mask,
                                since,
                                issued,
                                watchdog: Some(err::KERR_TIMED_OUT),
                            };
                        }
                        return Ok(());
                    }
                    // SAFETY: RPROC_STOP takes the processor mask and no buffer.
                    let r = unsafe {
                        hal_io(
                            RPROC_STOP,
                            Io {
                                a: mask,
                                ..Io::default()
                            },
                        )
                    }
                    .0;
                    if r < 0 {
                        self.fail("processor stop", r);
                        return Ok(());
                    }
                    self.phase = Phase::Recover {
                        mask,
                        since: now,
                        issued: true,
                        watchdog: None,
                    };
                } else {
                    match recovery::stop_progress(processor_stop_state(), now.saturating_sub(since))
                    {
                        StopProgress::Pending(error) => {
                            if watchdog.is_none() && error.is_some() {
                                dprintln!(
                                    "[hwhub] t={}ms processor stop watchdog {:?}; quarantined, still observing",
                                    now,
                                    error
                                );
                                self.phase = Phase::Recover {
                                    mask,
                                    since,
                                    issued,
                                    watchdog: error,
                                };
                            }
                        }
                        StopProgress::Failed(error) => {
                            self.fail("processor stop completion", error)
                        }
                        StopProgress::Complete(generations) => {
                            dprintln!(
                                "[hwhub] t={}ms processor stop acknowledged mask {} generations {}/{} after {}ms",
                                now,
                                mask,
                                generations[0],
                                generations[1],
                                now.saturating_sub(since)
                            );
                            self.router.restart_quiesced();
                            self.notify_adsp(ssctl::Event::AfterShutdown);
                            // fatal_seen stays: a FATAL bit the kernel left set is not a new fault
                            if mask & 2 != 0 {
                                self.adsp = adsp::Adsp::Idle;
                                self.phase = Phase::Adsp;
                            } else if self.router.radio_attached() {
                                self.boot_modem();
                            } else {
                                self.phase = Phase::Ipa;
                            }
                        }
                    }
                }
            }
            Phase::Running
            | Phase::Shutdown { .. }
            | Phase::Failed
            | Phase::Disabled
            | Phase::Absent => {}
        }
        Ok(())
    }

    fn boot(&mut self) {
        // SAFETY: RMTFS_MEM operation zero only writes the descriptor with region information.
        let (r, info) = unsafe { hal_io(RMTFS_MEM, Io::default()) };
        if r < 0 {
            self.fail("rmtfs carve-out", r);
            return;
        }
        dprintln!(
            "[hwhub] rmtfs buffer {:#x} + {:#x}; booting the modem",
            info.b,
            info.len
        );
        if info.len < 0x20_0000 {
            self.fail("rmtfs carve-out size", err::KERR_ARGUMENT);
            return;
        }
        let images = core::mem::take(&mut self.images);
        if let Err(e) = self.router.attach_radio(images, info.b as u64) {
            self.fail("radio storage attachment", e);
            return;
        }
        self.router.ipa_up = self.ipa_up;
        self.boot_modem();
    }

    fn boot_modem(&mut self) {
        if cfg!(feature = "radio-disabled") {
            self.phase = Phase::Disabled;
            return;
        }
        self.notify_adsp(ssctl::Event::BeforePowerup);
        // SAFETY: BOOT takes no arguments or buffers.
        let r = unsafe { hal(BOOT, core::ptr::null_mut(), core::ptr::null_mut()) };
        if r < 0 {
            self.fail("modem boot start", r);
            return;
        }
        self.phase = Phase::Wait(time::millis());
    }

    fn snapshot(&self) -> Snapshot {
        let phase = match self.phase {
            Phase::Adsp => hub_ipc::phase::ADSP,
            Phase::Ipa => hub_ipc::phase::IPA,
            Phase::Read(_) => hub_ipc::phase::READ,
            Phase::Wait(_) => hub_ipc::phase::WAIT,
            Phase::Running => hub_ipc::phase::RUNNING,
            Phase::Shutdown { .. } => hub_ipc::phase::RECOVER,
            Phase::Recover { .. } => hub_ipc::phase::RECOVER,
            Phase::Failed => hub_ipc::phase::FAILED,
            Phase::Disabled => hub_ipc::phase::DISABLED,
            Phase::Absent => hub_ipc::phase::ABSENT,
        };
        let adsp = match self.adsp {
            adsp::Adsp::Idle => hub_ipc::adsp::IDLE,
            adsp::Adsp::Loading(_) => hub_ipc::adsp::LOADING,
            adsp::Adsp::Booting(_) => hub_ipc::adsp::WAITING,
            adsp::Adsp::Up => hub_ipc::adsp::UP,
            adsp::Adsp::Stale | adsp::Adsp::Failed => hub_ipc::adsp::FAILED,
        };
        Snapshot {
            state: self.router.state(),
            phase,
            adsp,
            mss: state().unwrap_or_default(),
            adsp_state: adsp::processor_state().unwrap_or_default(),
            recovery_attempt: self.recovery_attempt,
            recovery_error: match self.phase {
                Phase::Recover { watchdog, .. } => watchdog.unwrap_or(0),
                Phase::Shutdown { .. } => 0,
                _ => self.recovery_error,
            },
            generation: self.router.generation(),
            no_hardware: matches!(self.phase, Phase::Absent),
        }
    }

    /// The one place that restarts the modem (Linux remoteproc: one owner, one recovery at a time). Every request
    /// is logged with its reason. Only the modem restarts (an ADSP fault doesn't restart it: `run`). While the modem
    /// has been running less than RESTART_HOLD_MS, only a crash restarts it. A running modem is asked to shut down
    /// first (SSCTL), and the ADSP hears "modem before shutdown".
    fn recover(&mut self, reason: &str, crashed: bool) -> bool {
        let now = time::millis();
        if matches!(
            self.phase,
            Phase::Recover { .. } | Phase::Shutdown { .. } | Phase::Absent
        ) {
            dprintln!(
                "[hwhub] t={}ms recovery requested ({}): one is running",
                now,
                reason
            );
            return false;
        }
        let running = matches!(self.phase, Phase::Running);
        if running && !crashed && now.saturating_sub(self.running_since) < RESTART_HOLD_MS {
            dprintln!(
                "[hwhub] t={}ms recovery requested ({}): ignored, the modem restarted {} ms ago",
                now,
                reason,
                now.saturating_sub(self.running_since)
            );
            return false;
        }
        dprintln!(
            "[hwhub] t={}ms recovery: {}{}",
            now,
            reason,
            if crashed { " (crashed)" } else { "" }
        );
        self.notify_adsp(ssctl::Event::BeforeShutdown);
        if running && !crashed && self.router.request_modem_shutdown(&mut Hal) {
            self.phase = Phase::Shutdown {
                until: now + SHUTDOWN_WAIT_MS,
            };
            return true;
        }
        self.begin_recovery();
        true
    }

    /// The ADSP hears what the modem does, only while it runs (a dead ADSP's queue would hold up the modem's)
    fn notify_adsp(&mut self, event: ssctl::Event) {
        if matches!(self.adsp, adsp::Adsp::Up) {
            self.router.notify_adsp(&mut Hal, event);
        }
    }

    /// The stop itself: the router lets the modem go, then after the attempt's backoff the kernel stop.
    fn begin_recovery(&mut self) {
        self.router.lost();
        self.healthy_since = 0;
        if self.recovery_attempt >= 3 {
            dprintln!("[hwhub] recovery: 3 attempts failed; trying again in 5 minutes");
            self.phase = Phase::Failed;
            self.failed_at = time::millis();
            return;
        }
        let delay = [1000, 3000, 10_000][usize::from(self.recovery_attempt)];
        self.recovery_attempt += 1;
        self.recovery_error = 0;
        let now = time::millis();
        self.quiesce_until = now + 2000;
        self.phase = Phase::Recover {
            mask: 1,
            since: now + delay,
            issued: false,
            watchdog: None,
        };
        dprintln!(
            "[hwhub] t={}ms recovery attempt {} mask 1",
            now,
            self.recovery_attempt
        );
    }

    fn fail(&mut self, what: &str, error: i32) {
        dprintln!("[hwhub] {} failed ({})", what, error);
        self.phase = Phase::Failed;
        self.failed_at = time::millis();
        self.recovery_error = error;
        if self.recovery_attempt < 3 {
            self.recover(what, true); // the modem isn't running: no shutdown to ask for, no window
        } else {
            self.router.lost();
        }
        self.recovery_error = error;
    }

    fn run(&mut self) {
        if matches!(self.phase, Phase::Recover { .. } | Phase::Absent) {
            return;
        }
        let now = time::millis();
        if matches!(self.phase, Phase::Failed)
            && self.failed_at != 0
            && now.saturating_sub(self.failed_at) >= RETRY_FAILED_MS
            // a failed kernel stop left memory quarantined: no automatic retry
            && matches!(processor_stop_state(), Ok([0 | 2, _, _, _]))
        {
            self.recovery_attempt = 0;
            self.failed_at = 0;
            self.recover("automatic retry after a failed recovery", true);
            return;
        }
        if let Phase::Shutdown { until } = self.phase {
            self.pump_modem();
            let bits = state().map_or(0, |s| s[2]);
            match self.router.modem_shutdown() {
                Some((_, true)) => {
                    dprintln!("[hwhub] t={}ms the modem is ready to shut down", now);
                    self.begin_recovery();
                }
                _ if bits & (SHUTDOWN_ACK | FATAL) != 0 => {
                    let what = if bits & FATAL != 0 {
                        "crashed while shutting down"
                    } else {
                        "acknowledged the shutdown"
                    };
                    dprintln!("[hwhub] t={}ms the modem {} ({:#x})", now, what, bits);
                    self.begin_recovery();
                }
                _ if now >= until => {
                    dprintln!(
                        "[hwhub] t={}ms no shutdown-ready from the modem in {} ms: stopping it (the kernel asks again)",
                        now,
                        SHUTDOWN_WAIT_MS
                    );
                    self.begin_recovery();
                }
                _ => {}
            }
        }
        if matches!(self.phase, Phase::Running) {
            let st = match state() {
                Ok(s) => s,
                Err(e) => {
                    dprintln!("[hwhub] modem state unavailable ({})", e);
                    self.recover("modem state unreadable", true);
                    return;
                }
            };
            if recovery::rising(&mut self.fatal_seen, st[2] & FATAL != 0) {
                self.recover("modem FATAL", true);
                return;
            }
            // an ADSP fault doesn't restart the modem (Linux restarts only the processor that failed): audio is
            // unavailable, calls and data keep going
            if matches!(self.adsp, adsp::Adsp::Up)
                && !matches!(adsp::processor_state(), Ok(s) if s[2] & FATAL == 0 && s[1] == 0)
            {
                dprintln!(
                    "[hwhub] ADSP failed ({:?}): audio unavailable; the modem stays up",
                    adsp::processor_state()
                );
                self.adsp = adsp::Adsp::Failed;
            }
            if self.healthy_since == 0 {
                self.healthy_since = now;
            }
            if now.saturating_sub(self.healthy_since) >= 60_000 {
                self.recovery_attempt = 0;
            }
            self.pump_modem();
            if now.saturating_sub(self.last_log) >= STATUS_LOG_MS {
                self.last_log = now;
                dprintln!(
                    "[hwhub] {} s: slave-kernel {:#x}, IPA {}, recovery {}",
                    now.saturating_sub(self.t0) / 1000,
                    st[2],
                    self.ipa_up,
                    self.recovery_attempt
                );
            }
        }
        for _ in 0..32 {
            let Some(n) = adsp::recv(adsp::RING_QRTR, &mut self.rx) else {
                break;
            };
            self.router.handle(&mut Hal, &self.rx[..n]);
        }
        // S3: the hub retries its shared ordered outbox on its own 10 ms timer. Cellular's
        // begin_tick/tick/poll run on MODEMD's timer; their old cross-process ordering is no longer imposed.
        self.router.retry(&mut Hal, now);
        self.router.tick(&mut Hal, now);
    }

    /// The modem's QRTR packets into the router.
    fn pump_modem(&mut self) {
        for _ in 0..32 {
            // SAFETY: RECV writes at most the live receive buffer capacity; HWHUB is its only reader.
            let n = unsafe { hal(RECV, self.rx.as_mut_ptr(), self.rx.len() as *mut u8) };
            if n == NOT_READY {
                break;
            }
            if n < 0 {
                dprintln!("[hwhub] receive failed ({})", n);
                break;
            }
            self.router.handle(&mut Hal, &self.rx[..n as usize]);
        }
    }

    fn events(&mut self) {
        while let Some(event) = self.router.next_event() {
            for client in &mut self.clients {
                let interested = match &event {
                    Event::Packet { dst_port, .. } => {
                        client.bindings.iter().any(|b| b.1 == *dst_port)
                    }
                    Event::ServerUp(s) | Event::ServerDown(s) => {
                        client.bindings.iter().any(|b| b.0 == s.service)
                    }
                    _ => true,
                };
                if interested {
                    client.push(event.clone());
                }
            }
        }
        let snapshot = self.snapshot();
        if self.published != snapshot {
            self.published = snapshot;
            for client in &mut self.clients {
                client.push(Event::Status(snapshot));
            }
        }
        for client in &mut self.clients {
            if let Some(msg) = client.receive.take() {
                match msg.des_max_len(0) {
                    Ok(max) => match client.queue.batch(max) {
                        Some(batch) => {
                            let result = msg.write8(0, &batch, 0).map_or_else(|e| e, |_| 0);
                            msg.complete(result);
                        }
                        None if !client.queue.is_empty() => msg.complete(-9),
                        None => client.receive = Some(msg),
                    },
                    Err(e) => msg.complete(e),
                }
            }
        }
    }
}

impl Service for Daemon {
    type Session = u32;
    fn connect(&mut self, v: TVersion) -> Result<u32, i32> {
        if v.major as i8 != hub_ipc::VERSION.0 {
            return Err(err::KERR_NOT_SUPPORTED);
        }
        if self.clients.len() >= 16 {
            return Err(err::KERR_IN_USE);
        }
        self.next_session = self.next_session.wrapping_add(1).max(1);
        let mut queue = SessionQueue::default();
        queue.push(Event::Status(self.snapshot()));
        self.clients.push(Client {
            id: self.next_session,
            bindings: Vec::new(),
            queue,
            receive: None,
            quiesced: self.router.generation(),
        });
        Ok(self.next_session)
    }
    fn message(&mut self, id: &mut u32, msg: Message) {
        let Some(i) = self.clients.iter().position(|c| c.id == *id) else {
            msg.complete(err::KERR_NOT_FOUND);
            return;
        };
        let result = match msg.function() {
            op::BIND => {
                let (service, port) = (msg.int(0) as u32, msg.int(1) as u32);
                if self
                    .clients
                    .iter()
                    .any(|c| c.id != *id && c.bindings.iter().any(|b| b.1 == port))
                {
                    err::KERR_IN_USE
                } else {
                    match self.router.bus(&mut Hal).bind(service, port) {
                        Err(e) => e,
                        Ok(()) => {
                            let c = &mut self.clients[i];
                            if !c.bindings.contains(&(service, port)) {
                                c.bindings.push((service, port));
                                c.push(Event::State(self.router.state()));
                                for s in &self.router.servers {
                                    if s.service == service {
                                        c.push(Event::ServerUp(*s));
                                    }
                                }
                                if matches!(self.phase, Phase::Running) {
                                    c.push(Event::RadioStarted);
                                }
                            }
                            0
                        }
                    }
                }
            }
            op::SEND => {
                let data = msg.des_len(0).and_then(|n| {
                    if n > 0x2100 {
                        Err(-9)
                    } else {
                        msg.read8_vec(0)
                    }
                });
                match data {
                    Ok(mut data) => {
                        let r = match hub_ipc::Send::decode(&data) {
                            Some(send)
                                if self.clients[i].bindings.iter().any(|b| b.1 == send.port)
                                    && !matches!(self.phase, Phase::Recover { .. }) =>
                            {
                                self.router.bus(&mut Hal).send(
                                    send.port,
                                    send.dst,
                                    send.payload,
                                    send.essential,
                                ) as i32
                            }
                            Some(_) => err::KERR_NOT_READY,
                            None => err::KERR_ARGUMENT,
                        };
                        hub_ipc::wipe(&mut data); // it may carry a SIM PIN
                        r
                    }
                    Err(e) => e,
                }
            }
            op::RECV => {
                if self.clients[i].receive.is_some() {
                    err::KERR_IN_USE
                } else if msg.des_max_len(0).is_ok_and(|n| n >= 12) {
                    self.clients[i].receive = Some(msg);
                    return;
                } else {
                    err::KERR_ARGUMENT
                }
            }
            op::CANCEL_RECV => {
                if let Some(receive) = self.clients[i].receive.take() {
                    receive.complete(-3);
                }
                0
            }
            op::STATUS => msg
                .write8(0, &self.snapshot().to_bytes(), 0)
                .map_or_else(|e| e, |_| 0),
            op::RECOVER => {
                let mask = msg.int(0) as u32;
                if mask == 0 || mask & !3 != 0 {
                    err::KERR_ARGUMENT
                } else if matches!(self.phase, Phase::Failed)
                    && !matches!(processor_stop_state(), Ok([0 | 2 | 3, _, _, _]))
                {
                    // C7 Retry never takes shutdown ownership from an unacknowledged kernel worker.
                    err::KERR_IN_USE
                } else {
                    if matches!(self.phase, Phase::Failed) {
                        self.recovery_attempt = 0;
                    }
                    // only the modem restarts; the reason comes from the client (MODEMD)
                    let _ = mask;
                    if self.recover(hub_ipc::reason::name(msg.int(1)), false) {
                        0
                    } else {
                        err::KERR_IN_USE // ignored (one running, or the restart window)
                    }
                }
            }
            op::QUIESCED => {
                if msg.int(0) as u32 == self.router.generation() {
                    self.clients[i].quiesced = msg.int(0) as u32;
                    0
                } else {
                    err::KERR_ARGUMENT
                }
            }
            _ => err::KERR_NOT_SUPPORTED,
        };
        msg.complete(result);
    }
    fn disconnect(&mut self, id: u32) -> ControlFlow<()> {
        if let Some(i) = self.clients.iter().position(|c| c.id == id) {
            let mut c = self.clients.remove(i);
            if let Some(msg) = c.receive.take() {
                msg.complete(-3);
            }
            for (_, port) in c.bindings {
                self.router.unbind(port);
            }
        }
        ControlFlow::Continue(())
    }
}
fn main() -> i32 {
    let server = match Server::create(hub_ipc::NAME) {
        Ok(s) => s,
        Err(_) => return 0,
    };
    let absent = state().is_err();
    let mut router = Router::support_only();
    router.radio_enabled = !cfg!(feature = "radio-disabled");
    router.trace = cfg!(feature = "qrtr-trace");
    let mut d = Daemon {
        phase: if absent { Phase::Absent } else { Phase::Adsp },
        images: Default::default(),
        router,
        rx: vec![0; 0x2000],
        adsp: adsp::Adsp::Idle,
        ipa_up: false,
        t0: 0,
        last_log: 0,
        fatal_seen: false,
        recovery_attempt: 0,
        adsp_reclaimed: false,
        recovery_error: 0,
        healthy_since: 0,
        running_since: 0,
        failed_at: 0,
        quiesce_until: 0,
        clients: Vec::new(),
        next_session: 0,
        published: Snapshot::default(),
    };
    dprintln!("[hwhub] serving NeoHub; no hardware {}", absent);
    rt::block_on(async {
        let mut receive = pin!(server.receive());
        let mut pump = hub::schedule::PumpDeadline::new(time::millis(), LOOP_MS);
        loop {
            if pump.due(time::millis()) {
                if let Err(e) = d.step() {
                    dprintln!("[hwhub] hardware unavailable ({})", e);
                    d.phase = Phase::Absent;
                }
                pump.completed(time::millis());
            }
            d.events();
            match rt::select(
                receive.as_mut(),
                time::delay(pump.wait_us(time::millis()).max(1)),
            )
            .await
            {
                Either::Left(Ok(msg)) => {
                    receive.set(server.receive());
                    let _ = server::dispatch(&mut d, msg);
                }
                Either::Left(Err(e)) => {
                    dprintln!("[hwhub] server receive failed ({})", e);
                    receive.set(server.receive());
                }
                Either::Right(()) => {}
            }
        }
    })
}
e32::main!(main);
