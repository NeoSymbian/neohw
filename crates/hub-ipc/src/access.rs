// SPDX-License-Identifier: Apache-2.0
//! (D11) What HWHUB lets a NeoHub client do, by the secure ID the kernel gives its messages: pure, so the rules are
//! tested on the host. QMI UIM carries SIM PINs, so only its owning daemon may bind it or send to it.
use crate::{Server, service_owner, sid};

pub const DENIED: i32 = -46; // KErrPermissionDenied
pub const IN_USE: i32 = -14; // KErrInUse
pub const NOT_READY: i32 = -18; // KErrNotReady

/// Sessions each daemon may hold, and all other processes together: others can't use up the daemons' sessions.
pub const PER_DAEMON: usize = 4;
pub const OTHERS: usize = 8;

/// MODEMD, WIFID and GNSSD: the processes that own a service.
pub fn daemon(sid: u32) -> bool {
    matches!(sid, sid::MODEMD | sid::WIFID | sid::GNSSD)
}

/// A new session for `sid`, which holds `of_sid` sessions already, while other processes hold `others`.
pub fn connect(sid: u32, of_sid: usize, others: usize) -> Result<(), i32> {
    let full = if daemon(sid) {
        of_sid >= PER_DAEMON
    } else {
        others >= OTHERS
    };
    if full { Err(IN_USE) } else { Ok(()) }
}

/// BIND `service` for `sid`: its owning daemon only; `port_taken` (another session bound the port) is in use.
pub fn bind(sid: u32, service: u32, port_taken: bool) -> Result<(), i32> {
    if service_owner(service) != Some(sid) {
        Err(DENIED)
    } else if port_taken {
        Err(IN_USE)
    } else {
        Ok(())
    }
}

/// SEND from `port` to `dst` (node, port) for a session with `bindings` (service, port): from a port it bound, to a
/// server of that service. A destination that is no server now (it went down, a recovery) is not ready.
pub fn send(
    bindings: &[(u32, u32)],
    servers: &[Server],
    port: u32,
    dst: (u32, u32),
) -> Result<(), i32> {
    let bound = |service| bindings.contains(&(service, port));
    if !bindings.iter().any(|b| b.1 == port) {
        return Err(DENIED);
    }
    match servers.iter().find(|s| (s.node, s.port) == dst) {
        None => Err(NOT_READY),
        Some(s) if bound(s.service) => Ok(()),
        Some(_) => Err(DENIED),
    }
}

/// RECOVER restarts the MSS (a call in progress drops): MODEMD only.
pub fn recover(sid: u32) -> bool {
    sid == sid::MODEMD
}

/// QUIESCED: a daemon that let go of the restarting subsystem.
pub fn quiesced(sid: u32) -> bool {
    daemon(sid)
}
