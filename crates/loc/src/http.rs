// SPDX-License-Identifier: Apache-2.0
//! The XTRA download: one plain-HTTP/1.0 GET and its response. The URLs come from the modem (Get Predicted Orbits
//! Data Source); only `http://<host>.izatcloud.net/...` ones are fetched, and the modem checks the file itself.
use alloc::format;
use alloc::vec::Vec;

/// The largest XTRA file we take (the modem's own limit is usually smaller).
pub const MAX_BODY: usize = 512 * 1024;
const ALLOWED: &str = ".izatcloud.net";

/// `http://host/path` -> `(host, path)`, only for the XTRA servers' hosts on port 80. The modem names `https://`
/// URLs (phone, 2026-11-20); the same files come over plain HTTP.
/// shortcut: HTTPS fetched as HTTP (no TLS on the phone yet; the modem checks the file), use TLS once there is one.
pub fn split_url(url: &str) -> Option<(&str, &str)> {
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))?;
    let (host, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let plain = host
        .bytes()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'.');
    let printable = path.bytes().all(|c| c.is_ascii_graphic());
    (plain && printable && host.len() > ALLOWED.len() && host.ends_with(ALLOWED))
        .then_some((host, path))
}

pub fn request(host: &str, path: &str) -> Vec<u8> {
    format!("GET {path} HTTP/1.0\r\nHost: {host}\r\nUser-Agent: NeoSymbian-GNSSD\r\nConnection: close\r\n\r\n")
        .into_bytes()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// no end of the headers
    Malformed,
    Status(u16),
    /// chunked transfer (HTTP/1.0 asked for none)
    Chunked,
    /// the body's length differs from Content-Length
    Length,
    TooLarge,
    Empty,
}

/// The body of a whole response (read until the server closed), checked.
pub fn body(resp: &[u8]) -> Result<&[u8], Error> {
    let end = resp
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or(Error::Malformed)?;
    let head = core::str::from_utf8(&resp[..end]).map_err(|_| Error::Malformed)?;
    let body = &resp[end + 4..];
    let mut lines = head.split("\r\n");
    let status = lines.next().ok_or(Error::Malformed)?;
    let mut parts = status.split(' ');
    if !parts.next().is_some_and(|v| v.starts_with("HTTP/1.")) {
        return Err(Error::Malformed);
    }
    let code: u16 = parts
        .next()
        .and_then(|c| c.parse().ok())
        .ok_or(Error::Malformed)?;
    if code != 200 {
        return Err(Error::Status(code));
    }
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("transfer-encoding") && !value.eq_ignore_ascii_case("identity")
        {
            return Err(Error::Chunked);
        }
        if name.eq_ignore_ascii_case("content-length") {
            let n: usize = value.parse().map_err(|_| Error::Malformed)?;
            if n > MAX_BODY {
                return Err(Error::TooLarge);
            }
            if n != body.len() {
                return Err(Error::Length);
            }
        }
    }
    match body.len() {
        0 => Err(Error::Empty),
        n if n > MAX_BODY => Err(Error::TooLarge),
        _ => Ok(body),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_xtra_hosts() {
        assert_eq!(
            split_url("http://xtrapath1.izatcloud.net/xtra3grcej.bin"),
            Some(("xtrapath1.izatcloud.net", "/xtra3grcej.bin"))
        );
        assert_eq!(
            split_url("https://xtrapath2.izatcloud.net/xtra3grcej.bin"),
            Some(("xtrapath2.izatcloud.net", "/xtra3grcej.bin"))
        );
        for bad in [
            "ftp://xtrapath1.izatcloud.net/x.bin",
            "http://izatcloud.net/x.bin",
            "http://.izatcloud.net/x.bin",
            "http://xtrapath1.izatcloud.net.evil.com/x.bin",
            "http://evil.com/xtrapath1.izatcloud.net",
            "http://xtrapath1.izatcloud.net:8080/x.bin",
            "http://user@xtrapath1.izatcloud.net/x.bin",
            "http://xtrapath1.izatcloud.net/x y",
            "http://xtrapath1.izatcloud.net/x\r\nHost: evil",
        ] {
            assert_eq!(split_url(bad), None, "{bad}");
        }
    }

    #[test]
    fn request_line() {
        let r = request("xtrapath1.izatcloud.net", "/xtra3grcej.bin");
        assert!(
            r.starts_with(b"GET /xtra3grcej.bin HTTP/1.0\r\nHost: xtrapath1.izatcloud.net\r\n")
        );
        assert!(r.ends_with(b"\r\n\r\n"));
    }

    #[test]
    fn bodies() {
        assert_eq!(
            body(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nabc"),
            Ok(&b"abc"[..])
        );
        assert_eq!(body(b"HTTP/1.0 200 OK\r\n\r\nabcd"), Ok(&b"abcd"[..]));
        assert_eq!(
            body(b"HTTP/1.1 404 Not Found\r\n\r\n"),
            Err(Error::Status(404))
        );
        assert_eq!(
            body(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nabc"),
            Err(Error::Length)
        );
        assert_eq!(
            body(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc"),
            Err(Error::Chunked)
        );
        assert_eq!(
            body(b"HTTP/1.1 200 OK\r\nContent-Length: 999999999\r\n\r\n"),
            Err(Error::TooLarge)
        );
        assert_eq!(body(b"HTTP/1.1 200 OK\r\n\r\n"), Err(Error::Empty));
        assert_eq!(body(b"HTTP/1.1 200 OK\r\nno end"), Err(Error::Malformed));
        assert_eq!(body(b"SSH-2.0\r\n\r\nabc"), Err(Error::Malformed));
    }
}
