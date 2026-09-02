//! A durable recording of the guest's GL command stream.
//!
//! Launching a workload and watching what it renders cannot separate the many
//! independent defects a young backend has: every run mixes them, and a run is
//! not a test. A trace does separate them. It is the exact bytes the guest
//! handed the device, so replaying it into a different backend is a controlled
//! experiment, and deleting commands from it until the failure disappears
//! names the shortest sequence that still reproduces.
//!
//! The file is deliberately dumb. It carries no host state, no timing and no
//! replies -- only the batches, in order, as the guest wrote them. Anything a
//! replay needs beyond that (which backend, which surface size) is an argument
//! to the replay, not a property of the recording.
//!
//! ```text
//! magic        "GEXGLTR\0"        8 bytes
//! format       u32                this file's layout, 1
//! protocol     u32                proto::PROTOCOL_VERSION when recorded
//! width        u32                surface the guest was given
//! height       u32
//! then, repeated until end of file:
//!   len        u32                bytes of command stream in this batch
//!   reply_cap  u32                reply area the guest offered
//!   bytes      len bytes, then zero padding to a multiple of four
//! ```
//!
//! Every field is little-endian, matching the wire protocol itself.

use crate::proto::PROTOCOL_VERSION;

/// The bytes at the head of a trace file.
pub const MAGIC: [u8; 8] = *b"GEXGLTR\0";

/// The layout described above. A reader refuses anything else rather than
/// guessing at a format it was not written for.
pub const FORMAT_VERSION: u32 = 1;

/// Size of the file header in bytes.
pub const HEADER: usize = 24;

/// What a trace says about the run that produced it.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Info {
    /// [`FORMAT_VERSION`] the file was written with.
    pub format_version: u32,
    /// The command ABI the guest and host had agreed on.
    pub protocol_version: u32,
    pub width: u32,
    pub height: u32,
}

/// One recorded submission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Batch {
    /// The command stream, exactly as the guest wrote it.
    pub bytes: Vec<u8>,
    /// How much room the guest had left for the reply. A replay that produces
    /// more than this would have been truncated in the original run, so it is
    /// recorded rather than assumed.
    pub reply_capacity: u32,
}

/// A whole trace, in memory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Trace {
    pub info: Info,
    pub batches: Vec<Batch>,
}

/// Why a byte string is not a trace this build can read.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// Shorter than the fixed header, or a record runs off the end.
    Truncated,
    /// The magic is not [`MAGIC`].
    NotATrace,
    /// A [`FORMAT_VERSION`] this build does not know.
    UnknownFormat(u32),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Truncated => f.write_str("trace ends inside a record"),
            Error::NotATrace => f.write_str("not a gex GL trace"),
            Error::UnknownFormat(v) => write!(f, "trace format {v} is from a newer build"),
        }
    }
}

impl std::error::Error for Error {}

/// Bytes of padding a record of `len` command bytes carries.
fn padding(len: usize) -> usize {
    len.wrapping_neg() & 3
}

/// The fixed header for a recording of a surface this size.
pub fn header(width: u32, height: u32) -> [u8; HEADER] {
    let mut out = [0u8; HEADER];
    out[..8].copy_from_slice(&MAGIC);
    out[8..12].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
    out[12..16].copy_from_slice(&PROTOCOL_VERSION.to_le_bytes());
    out[16..20].copy_from_slice(&width.to_le_bytes());
    out[20..24].copy_from_slice(&height.to_le_bytes());
    out
}

/// One record, ready to append to a file.
///
/// Returned rather than written so the caller decides where recordings go and
/// how failures there are reported; a device that cannot write its trace must
/// still run the guest.
pub fn record(batch: &[u8], reply_capacity: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + batch.len() + padding(batch.len()));
    out.extend_from_slice(&(batch.len() as u32).to_le_bytes());
    out.extend_from_slice(&reply_capacity.to_le_bytes());
    out.extend_from_slice(batch);
    out.extend(std::iter::repeat_n(0u8, padding(batch.len())));
    out
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

/// Read a whole trace.
pub fn parse(bytes: &[u8]) -> Result<Trace, Error> {
    if bytes.len() < HEADER {
        return Err(Error::Truncated);
    }
    if bytes[..8] != MAGIC {
        return Err(Error::NotATrace);
    }
    let format_version = u32_at(bytes, 8).ok_or(Error::Truncated)?;
    if format_version != FORMAT_VERSION {
        return Err(Error::UnknownFormat(format_version));
    }
    let info = Info {
        format_version,
        protocol_version: u32_at(bytes, 12).ok_or(Error::Truncated)?,
        width: u32_at(bytes, 16).ok_or(Error::Truncated)?,
        height: u32_at(bytes, 20).ok_or(Error::Truncated)?,
    };

    let mut batches = Vec::new();
    let mut at = HEADER;
    while at < bytes.len() {
        let len = u32_at(bytes, at).ok_or(Error::Truncated)? as usize;
        let reply_capacity = u32_at(bytes, at + 4).ok_or(Error::Truncated)?;
        let start = at + 8;
        let end = start.checked_add(len).ok_or(Error::Truncated)?;
        let padded = end.checked_add(padding(len)).ok_or(Error::Truncated)?;
        if padded > bytes.len() {
            return Err(Error::Truncated);
        }
        batches.push(Batch {
            bytes: bytes[start..end].to_vec(),
            reply_capacity,
        });
        at = padded;
    }
    Ok(Trace { info, batches })
}

/// Serialize a trace back out, byte for byte what a recording would have been.
pub fn write(trace: &Trace) -> Vec<u8> {
    let mut bytes = header(trace.info.width, trace.info.height).to_vec();
    for batch in &trace.batches {
        bytes.extend_from_slice(&record(&batch.bytes, batch.reply_capacity));
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{op, Batch as WireBatch};

    fn sample() -> Trace {
        let mut first = WireBatch::new();
        first.cmd(op::CONTEXT_CREATE, &[]);
        // Five bytes of source, so the record needs padding of its own.
        let mut second = WireBatch::new();
        second.raw(op::SHADER_SOURCE, &[1, 5], b"hello");
        Trace {
            info: Info {
                format_version: FORMAT_VERSION,
                protocol_version: PROTOCOL_VERSION,
                width: 640,
                height: 480,
            },
            batches: vec![
                Batch {
                    bytes: first.bytes().to_vec(),
                    reply_capacity: 4096,
                },
                Batch {
                    bytes: second.bytes().to_vec(),
                    reply_capacity: 4096,
                },
            ],
        }
    }

    #[test]
    fn a_trace_round_trips() {
        let trace = sample();
        assert_eq!(parse(&write(&trace)).unwrap(), trace);
    }

    #[test]
    fn an_empty_recording_is_still_a_trace() {
        let bytes = header(8, 8).to_vec();
        let trace = parse(&bytes).unwrap();
        assert!(trace.batches.is_empty());
        assert_eq!((trace.info.width, trace.info.height), (8, 8));
    }

    #[test]
    fn records_stay_four_byte_aligned() {
        let bytes = write(&sample());
        assert_eq!(bytes.len() % 4, 0);
    }

    #[test]
    fn a_recording_cut_short_is_an_error_rather_than_a_guess() {
        let trace = sample();
        let full = write(&trace);

        // A recording is complete at the header and after each whole record;
        // a capture killed mid-write ends anywhere else, and that must be an
        // error rather than a batch built from whatever bytes arrived.
        let mut complete = vec![HEADER];
        let mut at = HEADER;
        for batch in &trace.batches {
            at += record(&batch.bytes, batch.reply_capacity).len();
            complete.push(at);
        }

        for cut in 0..full.len() {
            let parsed = parse(&full[..cut]);
            if complete.contains(&cut) {
                assert!(parsed.is_ok(), "a whole trace of {cut} bytes was refused");
            } else {
                assert_eq!(
                    parsed,
                    Err(Error::Truncated),
                    "a trace cut to {cut} bytes was accepted"
                );
            }
        }
    }

    #[test]
    fn another_file_is_refused() {
        assert_eq!(parse(&[0u8; 64]), Err(Error::NotATrace));
        let mut wrong = write(&sample());
        wrong[8] = 99;
        assert_eq!(parse(&wrong), Err(Error::UnknownFormat(99)));
    }
}
