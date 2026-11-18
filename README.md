# neohw

NeoSymbian's hardware, user side: Rust for the Pixel 3a XL's (Qualcomm SDM670) modem, audio DSP, Wi-Fi and, later,
Bluetooth, on the 2010 Symbian user side (`armv5te-none-eabi`, `no_std` + `alloc`, the
[`e32`](https://github.com/NeoSymbian/e32-rs) crate). Renamed from `telephony` on 2026-11-18; `neolink` (Wi-Fi) was
merged in with its history.

Every driver's status, the processes and their IPC are in
[docs/hardware.md](https://github.com/NeoSymbian/docs/blob/main/hardware.md); the details per area in
[docs/hardware/](https://github.com/NeoSymbian/docs/tree/main/hardware) (modem.md, audio.md, wifi.md, bluetooth.md).
The kernel side (SCM, SMEM, SMP2P, GLINK, remote-processor boot, the audio codecs, the WCN3990 copy engines) is C++
in [kernel](https://github.com/NeoSymbian/kernel) under `baseport/arm64`.

## Layout

`crates/` is the host workspace: protocols, IPC contracts and sans-I/O state machines, tested on the Mac.
`symbian/` is the Symbian workspace: the EXEs (`exes.txt`), built only for armv5te.

| Crate / EXE | What |
|---|---|
| `modem-ipc` | The NeoModem contract (MODEMD <-> neoui, phone): calls, SIM/PIN, radio intent, ringing, SMS, recovery |
| `neoaudio` | The NeoAudio contract and its mixer, resampler and streams |
| `phone-rules`, `sms` | Dial strings, emergency numbers, caller matching, the blocklist; SMS PDUs |
| `qrtr` | QRTR and QMI wire codecs |
| `apr` | APR audio codecs and the audio state machine |
| `hub-ipc` | NeoHub operations, snapshots, event batches, session queues and the cellular Bus/Event boundary |
| `hub` | Node-1 QRTR routing, retry outbox, RAM EFS, locator/TFTP, IPA, SSCTL, recovery and WLFW driver |
| `modem` | Cellular QMI clients, status, calls, SIM/PIN, logs, blocklist and SMS; uses only the hub Bus/Event boundary |
| `ext4ro` | Read-only ext4 for firmware on the vendor partition |
| `wlfw` | The Wi-Fi firmware's QMI control service (WLFW, QMI 69); HWHUB drives it over its QRTR link |
| `wpa` | WPA2-PSK, supplicant side: PMK, PTK, the 4-way and group key handshakes (sans-I/O; CCMP, PSK, no MFP) |
| `net` | NeoNet: the IP stack (smoltcp: DHCP, DNS, TCP, UDP), its IPC protocol and the client (`client` feature) |
| `symbian/sys` | Shared HAL ABI, vendor ext4/firmware access and private-file saving |
| `symbian/hwhub` | HWHUB.EXE (0xE0003002): QRTR rings/router, firmware, MSS/ADSP boot, recovery and NeoHub |
| `symbian/modemd` | MODEMD.EXE (0xE0003001): cellular NeoHub client, NeoModem, APR audio and NeoAudio |
| `symbian/neonet` | NEONET.EXE (0xE0004001): the IP stack on the kernel's WlanLink frames, the NeoNet server |

Consumers pin these crates by git rev: neoui (`modem-ipc`, `neoaudio`, `phone-rules`, `wpa`, `net`), phone
(`modem-ipc`, `phone-rules`, `sms`).

## MODEMD notes

C7 implementation is on `feat/c7-complete-calls`; phone acceptance is still required. The server and all IPC consumers
must be updated together: v1 sessions are rejected. STATUS is a fixed little-endian snapshot; NOTIFY takes that same
writable descriptor in argument 0 and the last revision in argument 1. Each session may keep one notification
outstanding; CANCEL_NOTIFY and disconnect complete it with cancellation.

The call list is authoritative. Hold/resume/swap/merge report pending and QMI errors without claiming a state change
before the modem reports it. PIN values are bounded, never logged or persisted, and rejected PINs are never retried.
Airplane intent and ring settings are ordinary `C:\private\e0003001\settings.bin` settings; radio EFS still stays in RAM.

Ringtone playback shares the existing APR owner and board path. Its original PCM goes through a bounded kernel-owned
buffer, ASM and ADM; a buffer is republished only after matching DSP completion. Voice starts after ringtone close
and unmap. Wire facts come from primary [q6asm](https://github.com/torvalds/linux/blob/master/sound/soc/qcom/qdsp6/q6asm.c)
and [q6adm](https://github.com/torvalds/linux/blob/master/sound/soc/qcom/qdsp6/q6adm.c) references; QMI facts come from
[libqmi service descriptions](https://github.com/linux-mobile-broadband/libqmi/tree/main/data). No Linux implementation
was copied.

Recovery keeps NeoModem alive while the kernel stops the affected processors, invalidates their transport generation,
and reloads the existing read-only firmware path. RAM EFS survives. Three attempts use 1/3/10-second backoffs; exhausted
recovery remains visible with an explicit retry action. C6 audio ordering is preserved.

The connected-call return-to-LTE defect remains unresolved: existing traces do not establish its cause. No forced
radio cycle is presented as a fix. C7 must not be marked done until device acceptance resolves this and verifies the
new playback, hardware and recovery paths.

Validation uses the existing commands:

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
cd symbian
cargo fmt --all --check
cargo arm --workspace
cargo clippy --workspace --target armv5te-none-eabi -Zbuild-std=core,alloc --release -- -D warnings
```

Qualcomm firmware and board data are never stored here: they are read from the phone's own partitions at runtime.

## Licence

Apache-2.0 (`SPDX-License-Identifier: Apache-2.0` in every file). Code ported from the BSD-3-Clause linux-msm tools
keeps its original copyright line; [docs/hardware/modem.md](https://github.com/NeoSymbian/docs/blob/main/hardware/modem.md)
lists every outside source. Wi-Fi message ids and layouts are facts from Linux ath10k (ISC, Copyright (c) 2018
The Linux Foundation), cited in the source. `wpa` follows IEEE 802.11-2016 12.7 and uses RustCrypto's `sha1`, `hmac`,
`pbkdf2` and `aes-kw` (MIT/Apache-2.0).

## S3 process boundary

SYSSTART still starts MODEMD. It creates NeoModem and NeoAudio first, then connects to NeoHub, starting
HWHUB.EXE when the server is absent. Connection retries use the loop's deadline; firmware loading and recovery
run in HWHUB. HWHUB alone consumes MSS QRTR (HAL RECV 0x4e10) and ADSP QRTR (CHAN_RECV ring 1).
MODEMD alone consumes APR ring 2 until S4. PCM stays in its existing AudioMem buffers.

HWHUB starts the ADSP from `vendor_a` before IPA/EFS/RMTFS preparation (C5, C6). Its support-only router serves
locator/TFTP without advertising RMTFS or inventing a physical address; real radio resources attach later.
Radio preparation failure leaves ADSP QRTR servicing available. On Syborg HWHUB serves a no-hardware snapshot;
MODEMD retains NeoAudio's clock sink and rejects NeoModem connections once that state is known.

NeoHub v1 uses little-endian descriptors; integer arguments are Symbian IPC words:

| Op | Arguments and completion |
|---|---|
| 0 BIND | int0 service, int1 local port; session owns the port |
| 1 SEND | des0: port:u32, destination node:u32, destination port:u32, essential:u32 (0/1), length:u32, payload bytes; completion 1 physically sent, 0 otherwise |
| 2 RECV | writable des0: event batch; held until events arrive; one outstanding per session |
| 3 CANCEL_RECV | completes the session's outstanding receive with cancellation |
| 4 STATUS | writable des0: hub state, boot phase, processor states, recovery and generation snapshot |
| 5 RECOVER | int0 processor mask; current phone workaround forces MSS-only restart |
| 6 QUIESCED | int0 reset generation; acknowledges client state invalidation |

Event batches have a u32 count followed by u32 length and encoded event records. Records never cross batch
boundaries. Each session holds at most 64 events; overflow drops the oldest and reports a Dropped count.
Session close releases its ports and pending receive. Discovery is replayed when a client binds, so a late
MODEMD receives existing services. The pinned NeoModem and NeoAudio wire contracts stay unchanged.

Recovery sends Reset before waiting for every bound session's matching QUIESCED, or two seconds, then runs
the existing kernel stop acknowledgement/watchdog and 1/3/10-second backoff path. MODEMD clears Cellular
state and its audio/alert intents before acknowledging. RAM EFS survives; the current ADSP PAS shutdown
workaround still forces MSS-only recovery. Cellular retains logs, CLIR, messages/outgoing queue and user
intent; its blocklist clears on reset as before.

Both loops use a fixed 10 ms PumpDeadline checked before another IPC dispatch. HWHUB independently retries
the shared ordered 16-packet outbox at its one-second cadence and ticks IPA/WLFW. MODEMD independently runs
cellular intent, call/SMS timers and NAS polling. Cross-process scheduling replaces S2's exact
intent/retry/timers/IPA/poll order; event delivery may add a scheduling turn. Essential SEND still retains the
same transaction while queued. Nonessential SEND reports physical delivery. ADSP QRTR now makes one HAL send
attempt, replacing S2's 20 attempts with 5 ms sleeps; essential retries use the shared one-second cadence.
APR audio retains its existing send behavior. SEND/BIND/STATUS and scalar client calls are synchronous RPCs;
RECV is asynchronous and remains posted across server and timer wakeups.

The hub's diagnostic features are `radio-disabled` and `qrtr-trace` (Wi-Fi W0). Build from `symbian/` with
`cargo arm -p hwhub --features radio-disabled` to skip IPA/EFS/MSS startup while retaining ADSP services.
Hub logs use `[hwhub]`; cellular/audio logs use `[modemd]`.

The kernel GLINK HAL can still wait for a receive intent; firmware, storage and codec HAL calls are synchronous.
Host tests and ARM compilation do not establish phone timing, cold-boot, call, recovery or headset acceptance.
