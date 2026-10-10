//! Hard and configurable IPC limits.

use dgxtop_core::observation::MAX_RECORDS_PER_BATCH;

use super::error::IpcError;

/// Hard cap on one wire frame payload (1 MiB).
pub const MAX_WIRE_FRAME_BYTES: usize = 1 << 20;
/// Hard cap on the estimated decoded size of one message (4 MiB).
pub const MAX_DECODED_BYTES: usize = 4 << 20;
/// Hard cap on JSON container nesting.
pub const MAX_JSON_DEPTH: usize = 32;
/// Cap on structural JSON elements per frame. Tiny values (`"a",`) expand to
/// tens of bytes each once decoded, so the element count — not the wire size
/// alone — bounds the transient allocation of a decode.
pub const MAX_JSON_VALUES: usize = 65_536;
/// Requests carry at most 256 slots (~3 KiB); a smaller frame cap keeps the
/// child's request buffer tiny.
pub const REQUEST_FRAME_BYTES: usize = 64 * 1024;
/// Hello frames carry a manifest only.
pub const HELLO_FRAME_BYTES: usize = 64 * 1024;

/// Validated limits for one IPC endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IpcLimits {
    max_frame_bytes: usize,
    max_decoded_bytes: usize,
    max_records: usize,
    max_depth: usize,
    max_values: usize,
}

impl Default for IpcLimits {
    fn default() -> Self {
        Self {
            max_frame_bytes: MAX_WIRE_FRAME_BYTES,
            max_decoded_bytes: MAX_DECODED_BYTES,
            max_records: MAX_RECORDS_PER_BATCH,
            max_depth: MAX_JSON_DEPTH,
            max_values: MAX_JSON_VALUES,
        }
    }
}

impl IpcLimits {
    /// Limits lower than (or equal to) the hard caps; never higher.
    pub fn new(
        max_frame_bytes: usize,
        max_decoded_bytes: usize,
        max_records: usize,
    ) -> Result<Self, IpcError> {
        if !(1..=MAX_WIRE_FRAME_BYTES).contains(&max_frame_bytes) {
            return Err(IpcError::InvalidLimits(format!(
                "max_frame_bytes must be 1..={MAX_WIRE_FRAME_BYTES}"
            )));
        }
        if !(max_frame_bytes..=MAX_DECODED_BYTES).contains(&max_decoded_bytes) {
            return Err(IpcError::InvalidLimits(format!(
                "max_decoded_bytes must be max_frame_bytes..={MAX_DECODED_BYTES}"
            )));
        }
        if !(1..=MAX_RECORDS_PER_BATCH).contains(&max_records) {
            return Err(IpcError::InvalidLimits(format!(
                "max_records must be 1..={MAX_RECORDS_PER_BATCH}"
            )));
        }
        Ok(Self {
            max_frame_bytes,
            max_decoded_bytes,
            max_records,
            max_depth: MAX_JSON_DEPTH,
            max_values: MAX_JSON_VALUES,
        })
    }

    /// Same limits with a smaller frame cap (requests, hello frames).
    pub fn with_frame_cap(self, cap: usize) -> Self {
        Self {
            max_frame_bytes: self.max_frame_bytes.min(cap.max(1)),
            ..self
        }
    }

    pub const fn max_frame_bytes(&self) -> usize {
        self.max_frame_bytes
    }

    pub const fn max_decoded_bytes(&self) -> usize {
        self.max_decoded_bytes
    }

    pub const fn max_records(&self) -> usize {
        self.max_records
    }

    pub const fn max_depth(&self) -> usize {
        self.max_depth
    }

    pub const fn max_values(&self) -> usize {
        self.max_values
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_never_exceed_hard_caps() {
        assert!(IpcLimits::new(MAX_WIRE_FRAME_BYTES + 1, MAX_DECODED_BYTES, 256).is_err());
        assert!(IpcLimits::new(0, MAX_DECODED_BYTES, 256).is_err());
        assert!(IpcLimits::new(4096, MAX_DECODED_BYTES + 1, 256).is_err());
        assert!(IpcLimits::new(8192, 4096, 256).is_err());
        assert!(IpcLimits::new(4096, 65_536, 0).is_err());
        assert!(IpcLimits::new(4096, 65_536, 257).is_err());
        let limits = IpcLimits::new(4096, 65_536, 16).unwrap();
        assert_eq!(limits.max_frame_bytes(), 4096);
        assert_eq!(limits.max_records(), 16);
        assert_eq!(limits.max_depth(), 32);
        assert_eq!(limits.with_frame_cap(1024).max_frame_bytes(), 1024);
        assert_eq!(limits.with_frame_cap(1 << 30).max_frame_bytes(), 4096);
    }
}
