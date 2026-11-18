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
| `modem-proto` | QRTR/QMI services and clients, RAM-backed EFS, calls, SMS, APR audio (to be split, docs hardware.md S2) |
| `ext4ro` | Read-only ext4 for firmware on the vendor partition |
| `wlfw` | The Wi-Fi firmware's QMI control service (WLFW, QMI 69); MODEMD drives it over its QRTR link |
| `wpa` | WPA2-PSK, supplicant side: PMK, PTK, the 4-way and group key handshakes (sans-I/O; CCMP, PSK, no MFP) |
| `net` | NeoNet: the IP stack (smoltcp: DHCP, DNS, TCP, UDP), its IPC protocol and the client (`client` feature) |
| `symbian/modemd` | MODEMD.EXE (0xE0003001): the modem, the ADSP, NeoModem and NeoAudio |
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
cargo clippy --offline -- -D warnings
cargo test --offline
cd symbian
cargo fmt --all --check
cargo arm --workspace --offline
```

Qualcomm firmware and board data are never stored here: they are read from the phone's own partitions at runtime.

## Licence

Apache-2.0 (`SPDX-License-Identifier: Apache-2.0` in every file). Code ported from the BSD-3-Clause linux-msm tools
keeps its original copyright line; [docs/hardware/modem.md](https://github.com/NeoSymbian/docs/blob/main/hardware/modem.md)
lists every outside source. Wi-Fi message ids and layouts are facts from Linux ath10k (ISC, Copyright (c) 2018
The Linux Foundation), cited in the source. `wpa` follows IEEE 802.11-2016 12.7 and uses RustCrypto's `sha1`, `hmac`,
`pbkdf2` and `aes-kw` (MIT/Apache-2.0).

## Independent ADSP servicing (driver-support prerequisite)

MODEMD remains the only consumer of the ADSP APR and QRTR rings. It starts the ADSP from `vendor_a` before
IPA/EFS/RMTFS preparation. An ADSP-only router serves locator/TFTP without advertising RMTFS or inventing its
physical address; real radio resources attach to the same router later. Radio preparation/boot failure does not
stop ADSP QRTR or call/loopback processing. Pending radio packets are discarded on radio failure so they cannot
block ADSP replies. This does not implement general PCM playback or a separate audio server.

The loop checks a fixed polling deadline before dispatching another IPC request. Receive batches are bounded,
and the userspace transport no longer adds retry sleeps after a failed HAL send. The kernel GLINK HAL can still
wait for a receive intent (up to 200 kernel sleep iterations). The deadline prevents IPC floods from resetting
the polling timer; synchronous transport, firmware, storage and codec HAL calls still need measured phone budgets
before PCM deadlines can be promised.

For the startup-only radio-disabled diagnostic image, build from `symbian/` with
`cargo arm --features radio-disabled`. It skips IPA/EFS/MSS startup and automatic online requests while leaving
ADSP/loopback servicing available. This is a build option, not a runtime airplane-mode API. Default builds retain
normal cellular startup. Validate cold boot without SIM, radio-disabled loopback, forced radio preparation failure
and normal two-way calls on the phone before using this candidate as the installed image.
