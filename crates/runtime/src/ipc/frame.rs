//! Incremental, bounded framing: 4-byte big-endian length + payload.
//!
//! The decoder never reads past the frame it is assembling (see
//! [`FrameDecoder::wanted`]), so a caller that has to pause (for example
//! while waiting for ingress credit) simply stops reading the pipe and no
//! bytes need to be buffered elsewhere. The body buffer is allocated only
//! after the declared length passed validation, and only for that length.

use std::io::{self, Read, Write};

use serde::Serialize;

use super::error::IpcError;

const PREFIX_BYTES: usize = 4;

/// Result of one [`FrameDecoder::push`].
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    /// `consumed` bytes were absorbed; the current prefix/body is incomplete.
    NeedMore { consumed: usize },
    /// The length prefix is complete and valid. The caller must now call
    /// [`FrameDecoder::admit_body`] or [`FrameDecoder::skip_body`].
    Header { consumed: usize, declared: usize },
    /// A complete payload.
    Frame { consumed: usize, payload: Vec<u8> },
    /// A skipped body has been fully discarded.
    Skipped { consumed: usize, declared: usize },
}

impl Step {
    pub const fn consumed(&self) -> usize {
        match self {
            Self::NeedMore { consumed }
            | Self::Header { consumed, .. }
            | Self::Frame { consumed, .. }
            | Self::Skipped { consumed, .. } => *consumed,
        }
    }
}

#[derive(Debug)]
enum State {
    Prefix {
        bytes: [u8; PREFIX_BYTES],
        filled: usize,
    },
    Pending {
        declared: usize,
    },
    Body {
        buffer: Vec<u8>,
        declared: usize,
    },
    Skip {
        remaining: usize,
        declared: usize,
    },
    Failed,
}

impl State {
    const fn fresh_prefix() -> Self {
        Self::Prefix {
            bytes: [0; PREFIX_BYTES],
            filled: 0,
        }
    }
}

/// Bounded incremental frame decoder.
#[derive(Debug)]
pub struct FrameDecoder {
    max_frame_bytes: usize,
    state: State,
}

impl FrameDecoder {
    pub fn new(max_frame_bytes: usize) -> Result<Self, IpcError> {
        if !(1..=super::limits::MAX_WIRE_FRAME_BYTES).contains(&max_frame_bytes) {
            return Err(IpcError::InvalidLimits(format!(
                "frame decoder limit must be 1..={}",
                super::limits::MAX_WIRE_FRAME_BYTES
            )));
        }
        Ok(Self {
            max_frame_bytes,
            state: State::fresh_prefix(),
        })
    }

    pub const fn max_frame_bytes(&self) -> usize {
        self.max_frame_bytes
    }

    /// Bytes still missing from the current prefix or body. `0` means a body
    /// decision is pending (or the decoder failed) and nothing must be read.
    pub fn wanted(&self) -> usize {
        match &self.state {
            State::Prefix { filled, .. } => PREFIX_BYTES - filled,
            State::Body { buffer, declared } => declared - buffer.len(),
            State::Skip { remaining, .. } => *remaining,
            State::Pending { .. } | State::Failed => 0,
        }
    }

    /// At a frame boundary (nothing partially received).
    pub fn is_idle(&self) -> bool {
        matches!(self.state, State::Prefix { filled: 0, .. })
    }

    /// A body buffer is allocated and partially filled.
    pub fn is_receiving_body(&self) -> bool {
        matches!(self.state, State::Body { .. })
    }

    /// Declared length of a frame whose body decision is pending.
    pub fn pending_declared(&self) -> Option<usize> {
        match self.state {
            State::Pending { declared } => Some(declared),
            _ => None,
        }
    }

    /// Capacity of the body buffer currently held (observable allocation).
    pub fn buffered_capacity(&self) -> usize {
        match &self.state {
            State::Body { buffer, .. } => buffer.capacity(),
            _ => 0,
        }
    }

    /// Absorbs bytes up to the end of the current prefix or body.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Step, IpcError> {
        match &mut self.state {
            State::Failed => Err(IpcError::Poisoned),
            State::Pending { .. } => Err(IpcError::DecisionPending),
            State::Prefix { bytes, filled } => {
                let take = (PREFIX_BYTES - *filled).min(chunk.len());
                bytes[*filled..*filled + take].copy_from_slice(&chunk[..take]);
                *filled += take;
                if *filled < PREFIX_BYTES {
                    return Ok(Step::NeedMore { consumed: take });
                }
                let declared = u32::from_be_bytes(*bytes);
                self.validate_declared(declared)
                    .map(|declared| Step::Header {
                        consumed: take,
                        declared,
                    })
            }
            State::Body { buffer, declared } => {
                let take = (*declared - buffer.len()).min(chunk.len());
                buffer.extend_from_slice(&chunk[..take]);
                if buffer.len() < *declared {
                    return Ok(Step::NeedMore { consumed: take });
                }
                let payload = std::mem::take(buffer);
                self.state = State::fresh_prefix();
                Ok(Step::Frame {
                    consumed: take,
                    payload,
                })
            }
            State::Skip {
                remaining,
                declared,
            } => {
                let take = (*remaining).min(chunk.len());
                *remaining -= take;
                if *remaining > 0 {
                    return Ok(Step::NeedMore { consumed: take });
                }
                let declared = *declared;
                self.state = State::fresh_prefix();
                Ok(Step::Skipped {
                    consumed: take,
                    declared,
                })
            }
        }
    }

    fn validate_declared(&mut self, declared: u32) -> Result<usize, IpcError> {
        let limit = self.max_frame_bytes;
        let error = match usize::try_from(declared) {
            Ok(0) => IpcError::EmptyFrame,
            Ok(length) if length <= limit => {
                self.state = State::Pending { declared: length };
                return Ok(length);
            }
            _ => IpcError::FrameTooLarge {
                declared: u64::from(declared),
                limit,
            },
        };
        self.state = State::Failed;
        Err(error)
    }

    /// Allocates exactly the declared length and starts receiving the body.
    pub fn admit_body(&mut self) -> Result<(), IpcError> {
        let State::Pending { declared } = self.state else {
            return Err(IpcError::DecisionPending);
        };
        let mut buffer = Vec::new();
        if buffer.try_reserve_exact(declared).is_err() {
            self.state = State::Failed;
            return Err(IpcError::AllocationFailed { bytes: declared });
        }
        self.state = State::Body { buffer, declared };
        Ok(())
    }

    /// Discards the declared body without storing it (late replies).
    pub fn skip_body(&mut self) -> Result<(), IpcError> {
        let State::Pending { declared } = self.state else {
            return Err(IpcError::DecisionPending);
        };
        self.state = State::Skip {
            remaining: declared,
            declared,
        };
        Ok(())
    }

    /// Convenience for callers that accept every valid body: feeds a whole
    /// chunk and returns the completed payloads (at most `max_frames`).
    pub fn feed(&mut self, chunk: &[u8], max_frames: usize) -> Result<Vec<Vec<u8>>, IpcError> {
        let mut frames = Vec::new();
        let mut offset = 0;
        loop {
            if self.pending_declared().is_some() {
                self.admit_body()?;
            }
            if offset == chunk.len() {
                return Ok(frames);
            }
            let step = self.push(&chunk[offset..])?;
            offset += step.consumed();
            if let Step::Frame { payload, .. } = step {
                if frames.len() == max_frames {
                    self.state = State::Failed;
                    return Err(IpcError::contract("more frames than allowed in one chunk"));
                }
                frames.push(payload);
            }
        }
    }

    /// End-of-stream check: a partial frame is an error, never an empty batch.
    pub fn finish(&self) -> Result<(), IpcError> {
        match &self.state {
            State::Prefix { filled: 0, .. } => Ok(()),
            State::Prefix { filled, .. } => Err(IpcError::TruncatedFrame {
                received: *filled,
                expected: PREFIX_BYTES,
            }),
            State::Pending { declared } => Err(IpcError::TruncatedFrame {
                received: 0,
                expected: *declared,
            }),
            State::Body { buffer, declared } => Err(IpcError::TruncatedFrame {
                received: buffer.len(),
                expected: *declared,
            }),
            State::Skip {
                remaining,
                declared,
            } => Err(IpcError::TruncatedFrame {
                received: declared - remaining,
                expected: *declared,
            }),
            State::Failed => Err(IpcError::Poisoned),
        }
    }
}

/// Serializes `message` as one frame (prefix + JSON). Serialization stops as
/// soon as the payload would exceed `max_frame_bytes`, so an oversized
/// message never materializes in memory.
pub fn encode_frame<T: Serialize>(
    message: &T,
    max_frame_bytes: usize,
) -> Result<Vec<u8>, IpcError> {
    let limit = max_frame_bytes.min(super::limits::MAX_WIRE_FRAME_BYTES);
    let mut writer = LimitedWriter {
        buffer: vec![0; PREFIX_BYTES],
        limit: limit.saturating_add(PREFIX_BYTES),
        exceeded: false,
    };
    if let Err(error) = serde_json::to_writer(&mut writer, message) {
        if writer.exceeded {
            return Err(IpcError::FrameTooLarge {
                declared: u64::try_from(writer.buffer.len()).unwrap_or(u64::MAX),
                limit,
            });
        }
        return Err(IpcError::Encode {
            message: dgxtop_core::text::sanitize_message(&error.to_string()),
        });
    }
    let payload_len = writer.buffer.len() - PREFIX_BYTES;
    if payload_len == 0 {
        return Err(IpcError::EmptyFrame);
    }
    let prefix = u32::try_from(payload_len).map_err(|_| IpcError::FrameTooLarge {
        declared: u64::try_from(payload_len).unwrap_or(u64::MAX),
        limit,
    })?;
    writer.buffer[..PREFIX_BYTES].copy_from_slice(&prefix.to_be_bytes());
    Ok(writer.buffer)
}

struct LimitedWriter {
    buffer: Vec<u8>,
    limit: usize,
    exceeded: bool,
}

impl Write for LimitedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.buffer.len().saturating_add(bytes.len()) > self.limit {
            self.exceeded = true;
            return Err(io::Error::other("frame limit exceeded"));
        }
        self.buffer.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Blocking read of one frame (adapter-host side). Reads exactly what the
/// decoder wants, so no bytes of the following frame are consumed. Returns
/// `Ok(None)` on a clean end of stream at a frame boundary.
pub fn read_frame_blocking(
    reader: &mut dyn Read,
    decoder: &mut FrameDecoder,
    scratch: &mut [u8],
) -> Result<Option<Vec<u8>>, IpcError> {
    if scratch.is_empty() {
        return Err(IpcError::InvalidLimits("empty scratch buffer".into()));
    }
    loop {
        if decoder.pending_declared().is_some() {
            decoder.admit_body()?;
        }
        let wanted = decoder.wanted().min(scratch.len());
        if wanted == 0 {
            return Err(IpcError::Poisoned);
        }
        let read = match reader.read(&mut scratch[..wanted]) {
            Ok(0) => {
                decoder.finish()?;
                return Ok(None);
            }
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(IpcError::io(&error)),
        };
        let mut offset = 0;
        while offset < read {
            let step = decoder.push(&scratch[offset..read])?;
            offset += step.consumed();
            match step {
                Step::Frame { payload, .. } => return Ok(Some(payload)),
                Step::Header { .. } => decoder.admit_body()?,
                Step::NeedMore { .. } | Step::Skipped { .. } => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn frame(payload: &[u8]) -> Vec<u8> {
        let mut bytes = u32::try_from(payload.len()).unwrap().to_be_bytes().to_vec();
        bytes.extend_from_slice(payload);
        bytes
    }

    #[test]
    fn decodes_back_to_back_frames_in_one_chunk() {
        let mut stream = frame(b"{\"a\":1}");
        stream.extend(frame("{\"b\":\"溫度\"}".as_bytes()));
        let mut decoder = FrameDecoder::new(1024).unwrap();
        let frames = decoder.feed(&stream, 16).unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[1], "{\"b\":\"溫度\"}".as_bytes());
        assert!(decoder.is_idle());
        decoder.finish().unwrap();
    }

    #[test]
    fn oversize_prefix_is_rejected_without_allocating() {
        let mut decoder = FrameDecoder::new(1024).unwrap();
        let error = decoder.push(&[0xff, 0xff, 0xff, 0xff]).unwrap_err();
        assert_eq!(
            error,
            IpcError::FrameTooLarge {
                declared: u64::from(u32::MAX),
                limit: 1024
            }
        );
        assert_eq!(decoder.buffered_capacity(), 0);
        // The decoder stays poisoned; the stream cannot resynchronize.
        assert_eq!(decoder.push(b"{}"), Err(IpcError::Poisoned));
        let mut just_over = FrameDecoder::new(1024).unwrap();
        assert!(matches!(
            just_over.push(&1025u32.to_be_bytes()),
            Err(IpcError::FrameTooLarge { declared: 1025, .. })
        ));
        assert_eq!(just_over.buffered_capacity(), 0);
    }

    #[test]
    fn body_allocation_is_exactly_the_declared_length() {
        let mut decoder = FrameDecoder::new(1024).unwrap();
        let step = decoder.push(&700u32.to_be_bytes()).unwrap();
        assert_eq!(
            step,
            Step::Header {
                consumed: 4,
                declared: 700
            }
        );
        assert_eq!(decoder.wanted(), 0);
        assert_eq!(decoder.push(b"x"), Err(IpcError::DecisionPending));
        decoder.admit_body().unwrap();
        assert_eq!(decoder.buffered_capacity(), 700);
        assert_eq!(decoder.wanted(), 700);
    }

    #[test]
    fn zero_length_frames_are_rejected() {
        let mut decoder = FrameDecoder::new(1024).unwrap();
        assert_eq!(decoder.push(&[0, 0, 0, 0]), Err(IpcError::EmptyFrame));
    }

    #[test]
    fn eof_inside_prefix_or_body_is_an_error() {
        let mut decoder = FrameDecoder::new(1024).unwrap();
        decoder.feed(&[0, 0], 1).unwrap();
        assert_eq!(
            decoder.finish(),
            Err(IpcError::TruncatedFrame {
                received: 2,
                expected: 4
            })
        );
        let mut decoder = FrameDecoder::new(1024).unwrap();
        decoder.feed(&frame(b"{\"x\":1}")[..7], 1).unwrap();
        assert_eq!(
            decoder.finish(),
            Err(IpcError::TruncatedFrame {
                received: 3,
                expected: 7
            })
        );
    }

    #[test]
    fn skipped_bodies_are_discarded_without_allocation() {
        let mut stream = frame(&[b'z'; 600]);
        stream.extend(frame(b"{}"));
        let mut decoder = FrameDecoder::new(1024).unwrap();
        let step = decoder.push(&stream).unwrap();
        assert!(matches!(step, Step::Header { declared: 600, .. }));
        decoder.skip_body().unwrap();
        let mut offset = step.consumed();
        let mut skipped = false;
        let mut frames = Vec::new();
        while offset < stream.len() {
            if !skipped {
                assert_eq!(decoder.buffered_capacity(), 0);
            }
            let step = decoder.push(&stream[offset..]).unwrap();
            offset += step.consumed();
            match step {
                Step::Skipped { declared, .. } => {
                    assert_eq!(declared, 600);
                    skipped = true;
                }
                Step::Header { .. } => decoder.admit_body().unwrap(),
                Step::Frame { payload, .. } => frames.push(payload),
                Step::NeedMore { .. } => {}
            }
        }
        assert!(skipped);
        assert_eq!(frames, vec![b"{}".to_vec()]);
    }

    #[test]
    fn encode_frame_round_trips_and_respects_the_cap() {
        let message = serde_json::json!({"type": "hello", "text": "溫度"});
        let encoded = encode_frame(&message, 1024).unwrap();
        let mut decoder = FrameDecoder::new(1024).unwrap();
        let frames = decoder.feed(&encoded, 1).unwrap();
        let back: serde_json::Value = serde_json::from_slice(&frames[0]).unwrap();
        assert_eq!(back, message);
        let large = serde_json::json!({"blob": "y".repeat(5000)});
        assert!(matches!(
            encode_frame(&large, 1024),
            Err(IpcError::FrameTooLarge { limit: 1024, .. })
        ));
    }

    #[test]
    fn blocking_reader_stops_at_frame_boundaries() {
        let mut stream = frame(b"{\"first\":true}");
        stream.extend(frame(b"{\"second\":true}"));
        let mut reader: &[u8] = &stream;
        let mut decoder = FrameDecoder::new(1024).unwrap();
        let mut scratch = [0u8; 3];
        let first = read_frame_blocking(&mut reader, &mut decoder, &mut scratch).unwrap();
        assert_eq!(first.unwrap(), b"{\"first\":true}");
        let second = read_frame_blocking(&mut reader, &mut decoder, &mut scratch).unwrap();
        assert_eq!(second.unwrap(), b"{\"second\":true}");
        assert_eq!(
            read_frame_blocking(&mut reader, &mut decoder, &mut scratch),
            Ok(None)
        );
        let truncated = frame(b"{\"x\":1}");
        let mut reader: &[u8] = &truncated[..6];
        let mut decoder = FrameDecoder::new(1024).unwrap();
        assert!(matches!(
            read_frame_blocking(&mut reader, &mut decoder, &mut scratch),
            Err(IpcError::TruncatedFrame { .. })
        ));
    }

    fn payload_strategy() -> impl Strategy<Value = Vec<u8>> {
        // Multi-byte UTF-8 content so splits land inside code points.
        proptest::collection::vec(
            prop_oneof![Just("a"), Just("溫"), Just("度"), Just("é"), Just("🚀")],
            1..40,
        )
        .prop_map(|parts| {
            let text: String = parts.concat();
            serde_json::to_vec(&serde_json::json!({ "text": text })).unwrap()
        })
    }

    proptest! {
        #[test]
        fn random_chunking_preserves_every_frame(
            payloads in proptest::collection::vec(payload_strategy(), 1..6),
            cuts in proptest::collection::vec(1usize..17, 1..64),
        ) {
            let mut stream = Vec::new();
            for payload in &payloads {
                stream.extend(frame(payload));
            }
            let mut decoder = FrameDecoder::new(4096).unwrap();
            let mut received = Vec::new();
            let mut offset = 0;
            let mut cut_index = 0;
            while offset < stream.len() {
                let size = cuts[cut_index % cuts.len()].min(stream.len() - offset);
                cut_index += 1;
                received.extend(decoder.feed(&stream[offset..offset + size], 64).unwrap());
                offset += size;
                prop_assert!(decoder.buffered_capacity() <= 4096);
            }
            decoder.finish().unwrap();
            prop_assert_eq!(&received, &payloads);
            for payload in &received {
                prop_assert!(std::str::from_utf8(payload).is_ok());
            }
            // Random cuts may miss a length prefix; single bytes split every one.
            let mut decoder = FrameDecoder::new(4096).unwrap();
            let mut byte_by_byte = Vec::new();
            for byte in &stream {
                byte_by_byte.extend(decoder.feed(std::slice::from_ref(byte), 64).unwrap());
            }
            decoder.finish().unwrap();
            prop_assert_eq!(&byte_by_byte, &payloads);
        }

        #[test]
        fn any_declared_length_over_the_limit_never_allocates(declared in 1025u32..=u32::MAX) {
            let mut decoder = FrameDecoder::new(1024).unwrap();
            let first = decoder.push(&declared.to_be_bytes()[..2]).unwrap();
            prop_assert_eq!(first, Step::NeedMore { consumed: 2 });
            let rejected = decoder.push(&declared.to_be_bytes()[2..]);
            let is_too_large = matches!(rejected, Err(IpcError::FrameTooLarge { .. }));
            prop_assert!(is_too_large);
            prop_assert_eq!(decoder.buffered_capacity(), 0);
        }
    }
}
