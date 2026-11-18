// SPDX-License-Identifier: Apache-2.0
//! Call audio and APR ring 2 (C6, A1-A8). HWHUB owns ADSP firmware and QRTR ring 1.
const NOT_READY: i32 = -18; // KErrNotReady
use alloc::vec::Vec;
use apr::Mode;
use e32::{dprintln, time};
use sys::{AUDIO, AUDIO_MEM, CHAN_RECV, CHAN_SEND, Io, hal_io};

pub const RING_APR: u32 = 2;

/// One message from kernel ring `ring` into `buf`: its length, or None when the ring is empty.
fn recv(buf: &mut [u8]) -> Option<usize> {
    // SAFETY: CHAN_RECV writes at most buf.len() bytes to the live receive slice.
    let (r, _) = unsafe {
        hal_io(
            CHAN_RECV,
            Io {
                a: RING_APR,
                b: 0,
                len: buf.len() as u32,
                buf: buf.as_mut_ptr() as u32,
            },
        )
    };
    if r == NOT_READY {
        return None;
    }
    if r < 0 {
        dprintln!("[audiod] adsp: receive on ring {} failed ({})", RING_APR, r);
        return None;
    }
    Some(r as usize)
}

/// One message to the ADSP on ring `ring` (KErrNotReady until it gave a receive intent: retried briefly).
fn send(data: &[u8]) -> Result<(), i32> {
    for _ in 0..20 {
        // SAFETY: CHAN_SEND reads data.len() bytes from the live packet slice.
        let (r, _) = unsafe {
            hal_io(
                CHAN_SEND,
                Io {
                    a: RING_APR,
                    b: 0,
                    len: data.len() as u32,
                    buf: data.as_ptr() as u32,
                },
            )
        };
        match r {
            NOT_READY => time::sleep(5_000), // e32 sleeps in microseconds: 5 ms
            r if r < 0 => return Err(r),
            _ => return Ok(()),
        }
    }
    Err(NOT_READY)
}

/// HAL Audio (the kernel's board codecs, a64_audio.cpp): 1 start(route), 2 stop, 3 route, 4 volume, 5 mic mute.
pub fn audio(op: u32, arg: u32) -> i32 {
    // SAFETY: AUDIO takes scalar operation/argument fields and no buffer.
    unsafe {
        hal_io(
            AUDIO,
            Io {
                a: op,
                b: arg,
                len: 0,
                buf: 0,
            },
        )
    }
    .0
}

/// The audio (C6): the APR state machine (apr) with its codecs step, the call buttons' settings (kept
/// across calls), and Dev tools' test (Some(loudspeaker) while it is wanted; its route never touches the call's).
pub struct CallAudio {
    pub apr: apr::Audio,
    pub muted: bool,
    pub speaker: bool,
    pub volume: u8,
    pub test: Option<bool>,
    pub ring: modem_ipc::Ring,
    ring_loaded: bool,
    /// NeoAudio's media: the kernel route (0 earpiece, 1 both amps, 2 bottom) and amp volume 0..=10
    pub media_route: u32,
    pub media_volume: u8,
    /// recording reads logged so far (the first few, for bring-up)
    rec_logged: u32,
    /// the kernel's headphone path (op 9) is on: it goes off before the codecs (the ADSP's MCLK feeds it)
    jack_on: bool,
    /// the kernel's headset-mic path (op 10) is on: off with the headphones
    hsmic_on: bool,
}

impl CallAudio {
    pub fn new() -> Self {
        CallAudio {
            apr: Default::default(),
            muted: false,
            speaker: false,
            volume: 10,
            test: None,
            ring: Default::default(),
            ring_loaded: false,
            media_route: 1,
            media_volume: 7,
            rec_logged: 0,
            jack_on: false,
            hsmic_on: false,
        }
    }

    /// The kernel region's address, for media (the ringtone's preparation finds it too).
    pub fn prepare_region(&mut self) -> Result<(), i32> {
        if self.apr.pcm_phys != 0 {
            return Ok(());
        }
        // SAFETY: This AUDIO_MEM query/publish/completion uses scalar fields and no userspace buffer.
        let (r, info) = unsafe { hal_io(AUDIO_MEM, Io::default()) };
        if r < 0 {
            return Err(r);
        }
        self.apr.pcm_phys = info.b;
        self.apr.pcm_generation = info.buf;
        Ok(())
    }

    /// Media runs on the ADSP: fill every free period with `fill` (48 kHz mono), copy it into its range of the
    /// kernel region, publish that range and send its ASM WRITE.
    pub fn media_feed(&mut self, mut fill: impl FnMut(&mut [i16])) {
        use apr::{self, MEDIA_PERIOD};
        let mut pcm = [0i16; MEDIA_PERIOD / 2];
        let mut bytes = [0u8; MEDIA_PERIOD];
        // on headphones the amps' volume does nothing: the media volume scales the PCM (the amps' -30..0 dB in 3 dB
        // steps, Q15)
        const GAIN: [i32; 11] = [
            1036, 1464, 2068, 2920, 4125, 5827, 8231, 11627, 16423, 23198, 32767,
        ];
        let gain = self
            .apr
            .jack_is()
            .then(|| GAIN[usize::from(self.media_volume.min(10))]);
        while let Some(slot) = self.apr.media_slot() {
            fill(&mut pcm);
            if let Some(g) = gain {
                for s in pcm.iter_mut() {
                    *s = ((i32::from(*s) * g) >> 15) as i16;
                }
            }
            for (b, s) in bytes.as_chunks_mut::<2>().0.iter_mut().zip(pcm) {
                *b = s.to_le_bytes();
            }
            let off = apr::Audio::media_offset(slot);
            // SAFETY: AUDIO_MEM copy-in reads the live PCM buffer for its stated byte length.
            let (r, _) = unsafe {
                hal_io(
                    AUDIO_MEM,
                    Io {
                        a: 1,
                        b: off,
                        len: MEDIA_PERIOD as u32,
                        buf: bytes.as_ptr() as u32,
                    },
                )
            };
            if r < 0 {
                dprintln!("[audiod] media: copy into slot {} -> {}", slot, r);
                return;
            }
            // SAFETY: This AUDIO_MEM query/publish/completion uses scalar fields and no userspace buffer.
            let (r, info) = unsafe {
                hal_io(
                    AUDIO_MEM,
                    Io {
                        a: 2,
                        b: off,
                        len: MEDIA_PERIOD as u32,
                        buf: 0,
                    },
                )
            };
            if r < 0 {
                dprintln!("[audiod] media: publish slot {} -> {}", slot, r);
                return;
            }
            let p = self.apr.media_write(slot, info.b, info.buf);
            if let Err(e) = send(&p) {
                dprintln!("[audiod] media: write slot {} not sent ({})", slot, e);
                self.apr.media_unsent(slot);
                return;
            }
        }
    }

    /// Prepare the bounded kernel region once before opening a ringtone stream. It is never copied while published.
    pub fn prepare_ring(&mut self) -> Result<(), i32> {
        if self.ring_loaded {
            return Ok(());
        }
        // SAFETY: This AUDIO_MEM query/publish/completion uses scalar fields and no userspace buffer.
        let (r, info) = unsafe { hal_io(AUDIO_MEM, Io::default()) };
        if r < 0 {
            return Err(r);
        }
        let pcm = apr::ring_pcm(self.ring.volume);
        // SAFETY: AUDIO_MEM copy-in reads the live PCM buffer for its stated byte length.
        let (r, _) = unsafe {
            hal_io(
                AUDIO_MEM,
                Io {
                    a: 1,
                    b: 0,
                    len: pcm.len() as u32,
                    buf: pcm.as_ptr() as u32,
                },
            )
        };
        if r < 0 {
            return Err(r);
        }
        self.apr.pcm_phys = info.b;
        self.apr.pcm_generation = info.buf;
        self.ring_loaded = true;
        Ok(())
    }

    /// Answers from the ADSP, then whatever the sequence does next (a packet out, or the codecs).
    pub fn pump(&mut self, buf: &mut [u8]) {
        if self.apr.is() == Mode::Off && self.apr.idle() {
            self.ring_loaded = false;
        }
        let mut log = |a: core::fmt::Arguments| dprintln!("[audiod] t={}ms {}", time::millis(), a);
        for _ in 0..32 {
            let Some(n) = recv(buf) else {
                break;
            };
            self.apr.handle(&buf[..n], &mut log);
        }
        loop {
            match self.apr.poll(time::millis(), &mut log) {
                apr::Out::Send(p) => {
                    if let Err(e) = send(&p) {
                        dprintln!("[audiod] apr: send failed ({})", e);
                        self.apr.send_refused();
                    }
                    return; // its answer comes on a later step
                }
                apr::Out::Publish => {
                    // SAFETY: This AUDIO_MEM query/publish/completion uses scalar fields and no userspace buffer.
                    let (r, info) = unsafe {
                        hal_io(
                            AUDIO_MEM,
                            Io {
                                a: 2,
                                ..Io::default()
                            },
                        )
                    };
                    self.apr
                        .publish_done(if r < 0 { Err(r) } else { Ok(info.b) });
                    if r < 0 {
                        return;
                    }
                }
                apr::Out::Complete(ticket, generation) => {
                    // SAFETY: This AUDIO_MEM query/publish/completion uses scalar fields and no userspace buffer.
                    let r = unsafe {
                        hal_io(
                            AUDIO_MEM,
                            Io {
                                a: 3,
                                b: ticket,
                                buf: generation,
                                ..Io::default()
                            },
                        )
                    }
                    .0;
                    if r < 0 {
                        dprintln!("[audiod] PCM completion failed {}", r);
                        return;
                    }
                }
                apr::Out::Codecs(on) => {
                    let r = if on {
                        let v = match self.apr.is() {
                            Mode::Ring => self.ring.volume,
                            Mode::Media | Mode::Duplex => self.media_volume,
                            _ => self.volume,
                        };
                        let r = audio(4, v as u32);
                        // the call's Mute is the call's: a recording hears the mic
                        let mute = audio(5, (self.muted && self.apr.is() == Mode::Call) as u32);
                        if r < 0 {
                            r
                        } else if mute < 0 {
                            mute
                        } else if self.apr.jack_is() {
                            // headphones: the mic on, both amps off (route 3), then the jack's path (op 9)
                            let r = audio(1, 3);
                            let j = audio(9, 1);
                            self.jack_on = j >= 0;
                            // recording from the headset: its mic's path too (op 10)
                            let m = if self.apr.headset_is() {
                                audio(10, 1)
                            } else {
                                0
                            };
                            self.hsmic_on = self.apr.headset_is() && m >= 0;
                            if r < 0 {
                                r
                            } else if j < 0 {
                                j
                            } else {
                                m
                            }
                        } else {
                            audio(1, self.route())
                        }
                    } else {
                        if core::mem::take(&mut self.hsmic_on) {
                            let m = audio(10, 0);
                            dprintln!("[audiod] audio: headset mic off -> {}", m);
                        }
                        if core::mem::take(&mut self.jack_on) {
                            let j = audio(9, 0);
                            dprintln!("[audiod] audio: headphones off -> {}", j);
                        }
                        audio(2, 0)
                    };
                    dprintln!(
                        "[audiod] audio: codecs {} -> {}",
                        if on { "on" } else { "off" },
                        r
                    );
                    self.apr.codecs_done(r >= 0, &mut log);
                }
                apr::Out::Nothing => return,
            }
        }
    }

    /// Recording runs on the ADSP: hand it every free read buffer, then release the filled ones and copy them out.
    /// What was recorded (48 kHz mono), in order.
    pub fn rec_feed(&mut self) -> Vec<i16> {
        use apr::{self, MEDIA_PERIOD};
        let mut mic = Vec::new();
        let mut bytes = [0u8; MEDIA_PERIOD];
        for (slot, ticket, generation, n) in self.apr.take_recorded() {
            // SAFETY: This AUDIO_MEM query/publish/completion uses scalar fields and no userspace buffer.
            let r = unsafe {
                hal_io(
                    AUDIO_MEM,
                    Io {
                        a: 3,
                        b: ticket,
                        buf: generation,
                        ..Io::default()
                    },
                )
            }
            .0;
            if r < 0 {
                dprintln!("[audiod] rec: release slot {} -> {}", slot, r);
                continue;
            }
            if n == 0 {
                continue;
            }
            // SAFETY: AUDIO_MEM copy-out writes the live period buffer, bounded by the APR period size.
            let (r, _) = unsafe {
                hal_io(
                    AUDIO_MEM,
                    Io {
                        a: 4,
                        b: apr::Audio::rec_offset(slot),
                        len: n,
                        buf: bytes.as_mut_ptr() as u32,
                    },
                )
            };
            if r < 0 {
                dprintln!("[audiod] rec: copy out slot {} -> {}", slot, r);
                continue;
            }
            if self.rec_logged < 8 {
                self.rec_logged += 1;
                dprintln!("[audiod] rec: slot {} gave {} bytes", slot, n);
            }
            mic.extend(
                bytes[..n as usize]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|b| i16::from_le_bytes(*b)),
            );
        }
        while let Some(slot) = self.apr.rec_slot() {
            let off = apr::Audio::rec_offset(slot);
            // SAFETY: This AUDIO_MEM query/publish/completion uses scalar fields and no userspace buffer.
            let (r, info) = unsafe {
                hal_io(
                    AUDIO_MEM,
                    Io {
                        a: 2,
                        b: off,
                        len: MEDIA_PERIOD as u32,
                        buf: 0,
                    },
                )
            };
            if r < 0 {
                dprintln!("[audiod] rec: publish slot {} -> {}", slot, r);
                break;
            }
            let p = self.apr.rec_read(slot, info.b, info.buf);
            if self.rec_logged < 4 {
                self.rec_logged += 1;
                dprintln!(
                    "[audiod] rec: read slot {} at +{:#x} ticket {} generation {}",
                    slot,
                    off,
                    info.b,
                    info.buf
                );
            }
            if let Err(e) = send(&p) {
                dprintln!("[audiod] rec: read slot {} not sent ({})", slot, e);
                self.apr.rec_unsent(slot);
                break;
            }
        }
        mic
    }

    /// The route the codecs take (the kernel's: 0 earpiece, 1 both amps, 2 bottom): the test's while the test
    /// runs, the ringtone's loudspeaker, media's own, else the call's Speaker button.
    pub fn route(&self) -> u32 {
        match self.apr.is() {
            Mode::Test => self.test.unwrap_or(true) as u32,
            Mode::Ring => 1,
            Mode::Media | Mode::Duplex if self.apr.jack_is() => 3,
            Mode::Media | Mode::Duplex => self.media_route,
            _ => self.speaker as u32,
        }
    }
}
