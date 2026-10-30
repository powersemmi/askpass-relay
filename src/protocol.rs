//! The wire format between `ask` and `serve`.
//!
//! Every message is one frame: a big-endian `u32` payload length, then the payload. The payload
//! starts with the protocol version and a message kind, followed by fields, each a big-endian
//! `u16` length and that many bytes. A frame that is longer than [`MAX_FRAME`], names an unknown
//! version or kind, carries the wrong number of fields or leaves bytes over is rejected.
//!
//! A request is written at [`REQUEST_VERSION`] 2, which adds the reason field; a version 1
//! request (without it) is still read. The answer has not changed since version 1, so it stays at
//! [`RESPONSE_VERSION`] 1 and an `ask` of either version reads it.

use std::fmt;
use std::io::{self, Read, Write};
use std::ops::RangeInclusive;

use thiserror::Error;
use zeroize::Zeroizing;

/// The version a request is written at.
pub const REQUEST_VERSION: u8 = 2;

/// The version an answer is written at.
pub const RESPONSE_VERSION: u8 = 1;

/// The largest payload either side accepts.
pub const MAX_FRAME: usize = 64 * 1024;

const KIND_REQUEST: u8 = 1;
const KIND_PASSWORD: u8 = 2;
const KIND_CANCELLED: u8 = 3;
const KIND_FAILED: u8 = 4;

/// A password request sent by `ask`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// The host name of the machine that asks.
    pub host: String,
    /// The user sudo runs for.
    pub user: String,
    /// The prompt sudo passed to the askpass program.
    pub prompt: String,
    /// The command line of the sudo process that asked.
    pub command: String,
    /// Why the caller runs the command, from `ASKPASS_RELAY_REASON`.
    pub reason: Option<String>,
}

/// The answer `serve` sends back.
#[derive(PartialEq, Eq)]
pub enum Response {
    /// The user typed a password.
    Password(Zeroizing<Vec<u8>>),
    /// The user cancelled the dialog, or it timed out.
    Cancelled,
    /// The relay could not ask; the text says why.
    Failed(String),
}

impl fmt::Debug for Response {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Password(_) => f.write_str("Password(<redacted>)"),
            Self::Cancelled => f.write_str("Cancelled"),
            Self::Failed(reason) => f.debug_tuple("Failed").field(reason).finish(),
        }
    }
}

/// A frame that could not be read or written.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ProtocolError {
    /// The underlying stream failed.
    #[error("i/o error: {0}")]
    Io(#[from] io::Error),
    /// The peer announced a payload longer than [`MAX_FRAME`].
    #[error("frame of {0} bytes exceeds the limit of {MAX_FRAME}")]
    TooLarge(usize),
    /// The peer speaks another protocol version.
    #[error("unsupported protocol version {0}")]
    UnsupportedVersion(u8),
    /// The message kind is unknown or not expected in this direction.
    #[error("unexpected message kind {0}")]
    UnexpectedKind(u8),
    /// The payload ended inside a field, or a field count is wrong.
    #[error("truncated frame")]
    Truncated,
    /// The payload carries bytes after its last field.
    #[error("trailing bytes after the last field")]
    TrailingBytes,
    /// A text field is not valid UTF-8.
    #[error("a text field is not valid UTF-8")]
    InvalidUtf8,
    /// A field is longer than a `u16` length can describe.
    #[error("field of {0} bytes is too long")]
    FieldTooLong(usize),
}

/// Writes a request frame.
///
/// # Errors
///
/// Fails when a field is longer than 65535 bytes or the stream fails.
pub fn write_request(out: &mut impl Write, request: &Request) -> Result<(), ProtocolError> {
    let mut payload = vec![REQUEST_VERSION, KIND_REQUEST];
    let reason = request.reason.as_deref().unwrap_or_default();
    for field in [
        &request.host,
        &request.user,
        &request.prompt,
        &request.command,
        reason,
    ] {
        push_field(&mut payload, field.as_bytes())?;
    }
    write_frame(out, &payload)
}

/// Reads a request frame.
///
/// # Errors
///
/// Fails on a malformed frame (see [`ProtocolError`]) or when the stream fails.
pub fn read_request(input: &mut impl Read) -> Result<Request, ProtocolError> {
    let payload = read_frame(input)?;
    let mut fields = Fields::new(&payload, 1..=REQUEST_VERSION)?;
    if fields.kind != KIND_REQUEST {
        return Err(ProtocolError::UnexpectedKind(fields.kind));
    }
    let request = Request {
        host: fields.text()?,
        user: fields.text()?,
        prompt: fields.text()?,
        command: fields.text()?,
        // Version 1 has no reason field; an empty one means no reason.
        reason: if fields.version >= 2 {
            Some(fields.text()?).filter(|reason| !reason.is_empty())
        } else {
            None
        },
    };
    fields.finish()?;
    Ok(request)
}

/// Writes a response frame. A password frame is built in a buffer that is wiped on drop.
///
/// # Errors
///
/// Fails when a field is longer than 65535 bytes or the stream fails.
pub fn write_response(out: &mut impl Write, response: &Response) -> Result<(), ProtocolError> {
    let mut payload = Zeroizing::new(Vec::with_capacity(512));
    match response {
        Response::Password(password) => {
            payload.extend_from_slice(&[RESPONSE_VERSION, KIND_PASSWORD]);
            push_field(&mut payload, password)?;
        }
        Response::Cancelled => payload.extend_from_slice(&[RESPONSE_VERSION, KIND_CANCELLED]),
        Response::Failed(reason) => {
            payload.extend_from_slice(&[RESPONSE_VERSION, KIND_FAILED]);
            push_field(&mut payload, reason.as_bytes())?;
        }
    }
    write_frame(out, &payload)
}

/// Reads a response frame.
///
/// # Errors
///
/// Fails on a malformed frame (see [`ProtocolError`]) or when the stream fails.
pub fn read_response(input: &mut impl Read) -> Result<Response, ProtocolError> {
    let payload = read_frame(input)?;
    let mut fields = Fields::new(&payload, RESPONSE_VERSION..=RESPONSE_VERSION)?;
    let response = match fields.kind {
        KIND_PASSWORD => Response::Password(Zeroizing::new(fields.bytes()?.to_vec())),
        KIND_CANCELLED => Response::Cancelled,
        KIND_FAILED => Response::Failed(fields.text()?),
        other => return Err(ProtocolError::UnexpectedKind(other)),
    };
    fields.finish()?;
    Ok(response)
}

fn push_field(payload: &mut Vec<u8>, field: &[u8]) -> Result<(), ProtocolError> {
    let len = u16::try_from(field.len()).map_err(|_| ProtocolError::FieldTooLong(field.len()))?;
    payload.extend_from_slice(&len.to_be_bytes());
    payload.extend_from_slice(field);
    Ok(())
}

fn write_frame(out: &mut impl Write, payload: &[u8]) -> Result<(), ProtocolError> {
    if payload.len() > MAX_FRAME {
        return Err(ProtocolError::TooLarge(payload.len()));
    }
    // The length fits in a u32 because it is at most MAX_FRAME.
    let len = u32::try_from(payload.len()).map_err(|_| ProtocolError::TooLarge(payload.len()))?;
    out.write_all(&len.to_be_bytes())?;
    out.write_all(payload)?;
    out.flush()?;
    Ok(())
}

fn read_frame(input: &mut impl Read) -> Result<Zeroizing<Vec<u8>>, ProtocolError> {
    let mut len = [0_u8; 4];
    input.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(ProtocolError::TooLarge(len));
    }
    let mut payload = Zeroizing::new(vec![0_u8; len]);
    input
        .read_exact(&mut payload)
        .map_err(|error| match error.kind() {
            io::ErrorKind::UnexpectedEof => ProtocolError::Truncated,
            _ => ProtocolError::Io(error),
        })?;
    Ok(payload)
}

struct Fields<'a> {
    version: u8,
    kind: u8,
    rest: &'a [u8],
}

impl<'a> Fields<'a> {
    fn new(payload: &'a [u8], accepted: RangeInclusive<u8>) -> Result<Self, ProtocolError> {
        match payload {
            [version, kind, rest @ ..] if accepted.contains(version) => Ok(Self {
                version: *version,
                kind: *kind,
                rest,
            }),
            [version, _, ..] => Err(ProtocolError::UnsupportedVersion(*version)),
            _ => Err(ProtocolError::Truncated),
        }
    }

    fn bytes(&mut self) -> Result<&'a [u8], ProtocolError> {
        let [hi, lo, rest @ ..] = self.rest else {
            return Err(ProtocolError::Truncated);
        };
        let len = usize::from(u16::from_be_bytes([*hi, *lo]));
        if rest.len() < len {
            return Err(ProtocolError::Truncated);
        }
        let (field, rest) = rest.split_at(len);
        self.rest = rest;
        Ok(field)
    }

    fn text(&mut self) -> Result<String, ProtocolError> {
        let bytes = self.bytes()?;
        String::from_utf8(bytes.to_vec()).map_err(|_| ProtocolError::InvalidUtf8)
    }

    fn finish(self) -> Result<(), ProtocolError> {
        if self.rest.is_empty() {
            Ok(())
        } else {
            Err(ProtocolError::TrailingBytes)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> Request {
        Request {
            host: "pro".into(),
            user: "alice".into(),
            prompt: "[sudo] password for alice: ".into(),
            command: "sudo -A pacman -Syu".into(),
            reason: Some("update the build host".into()),
        }
    }

    fn frame(payload: &[u8]) -> Vec<u8> {
        let mut out = u32::try_from(payload.len()).unwrap().to_be_bytes().to_vec();
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn request_round_trips() {
        let mut wire = Vec::new();
        write_request(&mut wire, &request()).unwrap();
        assert_eq!(read_request(&mut wire.as_slice()).unwrap(), request());
    }

    #[test]
    fn every_response_round_trips() {
        for response in [
            Response::Password(Zeroizing::new(b"p@ss word\nwith newline".to_vec())),
            Response::Cancelled,
            Response::Failed("no display".into()),
        ] {
            let mut wire = Vec::new();
            write_response(&mut wire, &response).unwrap();
            assert_eq!(read_response(&mut wire.as_slice()).unwrap(), response);
        }
    }

    #[test]
    fn request_frame_layout_is_stable() {
        let mut wire = Vec::new();
        let request = Request {
            host: "h".into(),
            user: "u".into(),
            prompt: String::new(),
            command: "c".into(),
            reason: Some("r".into()),
        };
        write_request(&mut wire, &request).unwrap();
        let expected = frame(&[2, 1, 0, 1, b'h', 0, 1, b'u', 0, 0, 0, 1, b'c', 0, 1, b'r']);
        assert_eq!(wire, expected);
    }

    #[test]
    fn request_without_a_reason_round_trips() {
        let request = Request {
            reason: None,
            ..request()
        };
        let mut wire = Vec::new();
        write_request(&mut wire, &request).unwrap();
        assert_eq!(read_request(&mut wire.as_slice()).unwrap(), request);
    }

    #[test]
    fn version_1_request_is_accepted_without_a_reason() {
        let wire = frame(&[1, 1, 0, 1, b'h', 0, 1, b'u', 0, 0, 0, 1, b'c']);
        let request = read_request(&mut wire.as_slice()).unwrap();
        assert_eq!(request.command, "c");
        assert_eq!(request.reason, None);
    }

    #[test]
    fn version_2_request_without_the_reason_field_is_truncated() {
        let wire = frame(&[2, 1, 0, 1, b'h', 0, 1, b'u', 0, 0, 0, 1, b'c']);
        let error = read_request(&mut wire.as_slice()).unwrap_err();
        assert!(matches!(error, ProtocolError::Truncated), "{error:?}");
    }

    #[test]
    fn response_frame_stays_at_version_1() {
        let mut wire = Vec::new();
        write_response(&mut wire, &Response::Cancelled).unwrap();
        assert_eq!(wire, frame(&[1, KIND_CANCELLED]));
    }

    #[test]
    fn oversized_frame_is_rejected_before_reading_it() {
        let wire = u32::try_from(MAX_FRAME + 1).unwrap().to_be_bytes();
        let error = read_request(&mut wire.as_slice()).unwrap_err();
        assert!(matches!(error, ProtocolError::TooLarge(len) if len == MAX_FRAME + 1));
    }

    #[test]
    fn other_version_is_rejected() {
        let wire = frame(&[3, 1]);
        let error = read_request(&mut wire.as_slice()).unwrap_err();
        assert!(matches!(error, ProtocolError::UnsupportedVersion(3)));
    }

    #[test]
    fn response_kind_is_not_a_request() {
        let wire = frame(&[1, KIND_CANCELLED]);
        let error = read_request(&mut wire.as_slice()).unwrap_err();
        assert!(matches!(
            error,
            ProtocolError::UnexpectedKind(KIND_CANCELLED)
        ));
    }

    #[test]
    fn missing_field_is_truncated() {
        let wire = frame(&[1, 1, 0, 1, b'h']);
        let error = read_request(&mut wire.as_slice()).unwrap_err();
        assert!(matches!(error, ProtocolError::Truncated));
    }

    #[test]
    fn field_longer_than_payload_is_truncated() {
        let wire = frame(&[1, 2, 0, 9, b'x']);
        let error = read_response(&mut wire.as_slice()).unwrap_err();
        assert!(matches!(error, ProtocolError::Truncated));
    }

    #[test]
    fn frame_shorter_than_announced_is_truncated() {
        let mut wire = 10_u32.to_be_bytes().to_vec();
        wire.extend_from_slice(&[1, 3]);
        let error = read_response(&mut wire.as_slice()).unwrap_err();
        assert!(matches!(error, ProtocolError::Truncated));
    }

    #[test]
    fn trailing_bytes_are_rejected() {
        let wire = frame(&[1, KIND_CANCELLED, 0]);
        let error = read_response(&mut wire.as_slice()).unwrap_err();
        assert!(matches!(error, ProtocolError::TrailingBytes));
    }

    #[test]
    fn invalid_utf8_is_rejected() {
        let wire = frame(&[1, 1, 0, 1, 0xff, 0, 0, 0, 0, 0, 0]);
        let error = read_request(&mut wire.as_slice()).unwrap_err();
        assert!(matches!(error, ProtocolError::InvalidUtf8));
    }

    #[test]
    fn debug_never_prints_the_password() {
        let response = Response::Password(Zeroizing::new(b"hunter2".to_vec()));
        assert_eq!(format!("{response:?}"), "Password(<redacted>)");
    }
}
