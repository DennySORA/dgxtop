//! Adapter-host IPC (`contracts/INTERFACES.md` §7/§10, `SECURITY.md` §4).
//!
//! Wire format: a 4-byte big-endian payload length followed by UTF-8 JSON.
//! The length is validated *before* any buffer is allocated; payloads are
//! limited to 1 MiB on the wire, 4 MiB decoded, 256 records and a JSON
//! nesting depth of 32. Decoding never panics; every rejection is a typed
//! [`IpcError`].

mod decode;
mod error;
mod frame;
mod limits;
mod size;

pub use decode::{
    HelloExpectation, decode_batch, decode_bounded, decode_catalog, decode_hello, decode_request,
    decode_shutdown_ack, scan_structure,
};
pub use error::IpcError;
pub use frame::{FrameDecoder, Step, encode_frame, read_frame_blocking};
pub use limits::{
    HELLO_FRAME_BYTES, IpcLimits, MAX_DECODED_BYTES, MAX_JSON_DEPTH, MAX_JSON_VALUES,
    MAX_WIRE_FRAME_BYTES, REQUEST_FRAME_BYTES,
};
pub use size::DecodedSize;
