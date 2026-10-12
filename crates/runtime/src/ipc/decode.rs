//! Bounded JSON decoding of IPC payloads.
//!
//! Order matters: UTF-8 validation and a structural pre-scan (nesting depth,
//! element count) run before serde allocates anything, then the typed decode,
//! then message-specific post-checks (record count, decoded-size estimate,
//! hello identity). Serde's own error text never reaches a terminal unsanitized.

use serde::de::DeserializeOwned;

use dgxtop_core::identity::AdapterId;
use dgxtop_core::observation::{
    CatalogReply, HostHello, IPC_PROTOCOL_VERSION, IpcRequest, MAX_CATALOG_PAGE,
    MAX_RECORDS_PER_BATCH, ObservationBatch, ShutdownAck,
};

use super::error::IpcError;
use super::limits::IpcLimits;
use super::size::DecodedSize;

/// Structural facts gathered by [`scan_structure`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Structure {
    pub max_depth: usize,
    pub elements: usize,
}

/// Walks the JSON text once without allocating. Brackets inside strings are
/// ignored and backslash escapes are honoured, so `"[[["` does not count as
/// nesting. Multi-byte UTF-8 sequences never contain ASCII bytes, so a byte
/// scan is exact on validated UTF-8.
pub fn scan_structure(text: &str, limits: &IpcLimits) -> Result<Structure, IpcError> {
    let mut depth = 0usize;
    let mut max_depth = 0usize;
    // Every value except the first in a container is preceded by a comma and
    // every container opens with a bracket, so this bounds the value count.
    let mut elements = 1usize;
    let bytes = text.as_bytes();
    let mut index = 0usize;
    while let Some(&byte) = bytes.get(index) {
        index += 1;
        match byte {
            // Most bytes are string content: jump over it in one search.
            b'"' => index = string_end(bytes, index),
            b'[' | b'{' => {
                depth += 1;
                elements += 1;
                if depth > limits.max_depth() {
                    return Err(IpcError::TooDeep {
                        limit: limits.max_depth(),
                    });
                }
                max_depth = max_depth.max(depth);
            }
            b']' | b'}' => depth = depth.saturating_sub(1),
            b',' => elements += 1,
            _ => {}
        }
        if elements > limits.max_values() {
            return Err(IpcError::TooManyValues {
                limit: limits.max_values(),
            });
        }
    }
    Ok(Structure {
        max_depth,
        elements,
    })
}

/// Index just past the closing quote of the string whose content starts at
/// `start` (the input length when it is unterminated: the decoder rejects it).
fn string_end(bytes: &[u8], start: usize) -> usize {
    let mut index = start;
    while let Some(rest) = bytes.get(index..) {
        match rest.iter().position(|byte| *byte == b'"' || *byte == b'\\') {
            Some(offset) if rest.get(offset) == Some(&b'\\') => index += offset + 2,
            Some(offset) => return index + offset + 1,
            None => break,
        }
    }
    bytes.len()
}

/// UTF-8 validation, structural pre-scan, then serde decode of `T`.
pub fn decode_bounded<T: DeserializeOwned>(
    payload: &[u8],
    limits: &IpcLimits,
) -> Result<T, IpcError> {
    if payload.len() > limits.max_frame_bytes() {
        return Err(IpcError::FrameTooLarge {
            declared: u64::try_from(payload.len()).unwrap_or(u64::MAX),
            limit: limits.max_frame_bytes(),
        });
    }
    let text = std::str::from_utf8(payload).map_err(|error| IpcError::InvalidUtf8 {
        valid_up_to: error.valid_up_to(),
    })?;
    scan_structure(text, limits)?;
    serde_json::from_str(text).map_err(|error| IpcError::malformed(&error.to_string()))
}

fn check_decoded_size<T: DecodedSize>(value: &T, limits: &IpcLimits) -> Result<(), IpcError> {
    let estimated = value.decoded_bytes();
    let limit = limits.max_decoded_bytes();
    if estimated > u64::try_from(limit).unwrap_or(u64::MAX) {
        return Err(IpcError::DecodedTooLarge { estimated, limit });
    }
    Ok(())
}

/// What the parent expects in the first frame of one child incarnation.
#[derive(Debug, Clone, Copy)]
pub struct HelloExpectation<'a> {
    pub adapter_id: &'a AdapterId,
    pub generation: u32,
    pub nonce: &'a str,
}

/// Decodes and verifies a hello: protocol version, adapter id, generation,
/// nonce and a valid manifest for the same adapter.
pub fn decode_hello(
    payload: &[u8],
    limits: &IpcLimits,
    expected: &HelloExpectation<'_>,
) -> Result<HostHello, IpcError> {
    let hello: HostHello = decode_bounded(payload, limits)?;
    check_decoded_size(&hello, limits)?;
    if hello.protocol_version != IPC_PROTOCOL_VERSION {
        return Err(IpcError::HelloMismatch {
            field: "protocol_version",
        });
    }
    if &hello.adapter_id != expected.adapter_id {
        return Err(IpcError::HelloMismatch {
            field: "adapter_id",
        });
    }
    if hello.generation != expected.generation {
        return Err(IpcError::HelloMismatch {
            field: "generation",
        });
    }
    if !constant_time_eq(hello.nonce.as_bytes(), expected.nonce.as_bytes()) {
        return Err(IpcError::HelloMismatch { field: "nonce" });
    }
    if &hello.manifest.adapter_id != expected.adapter_id {
        return Err(IpcError::HelloMismatch {
            field: "manifest.adapter_id",
        });
    }
    hello
        .manifest
        .validate()
        .map_err(|error| IpcError::contract(&error.to_string()))?;
    Ok(hello)
}

/// Decodes an observation batch and enforces the record and decoded-size
/// limits. Consistency with the in-flight plan is checked by the caller
/// (`ObservationBatch::validate_against`).
pub fn decode_batch(payload: &[u8], limits: &IpcLimits) -> Result<ObservationBatch, IpcError> {
    let batch: ObservationBatch = decode_bounded(payload, limits)?;
    let limit = limits.max_records().min(MAX_RECORDS_PER_BATCH);
    if batch.records.len() > limit {
        return Err(IpcError::TooManyRecords {
            count: batch.records.len(),
            limit,
        });
    }
    check_decoded_size(&batch, limits)?;
    Ok(batch)
}

/// Decodes a catalog page and validates its page bound.
pub fn decode_catalog(payload: &[u8], limits: &IpcLimits) -> Result<CatalogReply, IpcError> {
    let reply: CatalogReply = decode_bounded(payload, limits)?;
    reply
        .delta
        .validate(MAX_CATALOG_PAGE)
        .map_err(|error| IpcError::contract(&error.to_string()))?;
    check_decoded_size(&reply, limits)?;
    Ok(reply)
}

/// Decodes a shutdown acknowledgement.
pub fn decode_shutdown_ack(payload: &[u8], limits: &IpcLimits) -> Result<ShutdownAck, IpcError> {
    let ack: ShutdownAck = decode_bounded(payload, limits)?;
    check_decoded_size(&ack, limits)?;
    Ok(ack)
}

/// Decodes and validates a parent request (child side). Unknown operations
/// (including any action-like verb) fail at the `type` enum.
pub fn decode_request(payload: &[u8], limits: &IpcLimits) -> Result<IpcRequest, IpcError> {
    let request: IpcRequest = decode_bounded(payload, limits)?;
    request
        .validate()
        .map_err(|error| IpcError::contract(&error.to_string()))?;
    check_decoded_size(&request, limits)?;
    Ok(request)
}

/// Comparison whose duration does not depend on where the inputs differ.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0u8, |difference, (a, b)| difference | (a ^ b))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::observation::{
        AdapterManifest, AdapterOperation, BatchStatus, CatalogDelta, CatalogTag, HelloTag,
        IpcRequestKind, Isolation, ShutdownTag,
    };
    use dgxtop_core::quality::Namespace;
    use dgxtop_core::testing::{ObservationBuilder, adapter_id, batch_of, metric_key, read_group};
    use proptest::prelude::*;
    use uuid::Uuid;

    fn manifest(adapter: &str) -> AdapterManifest {
        AdapterManifest {
            api_version: 1,
            adapter_id: adapter_id(adapter),
            implementation: "FakeAdapter".into(),
            isolation: Isolation::PersistentProcess,
            operations: vec![
                AdapterOperation::Discover,
                AdapterOperation::Read,
                AdapterOperation::Shutdown,
            ],
            max_inflight: 1,
            runtime_library: None,
            minimum_sample_ms: 100,
            source_limit: 16,
            sample_namespace: Namespace::Live,
        }
    }

    fn hello(adapter: &str, generation: u32, nonce: &str) -> HostHello {
        HostHello {
            kind: HelloTag::Hello,
            protocol_version: 1,
            adapter_id: adapter_id(adapter),
            generation,
            nonce: nonce.into(),
            manifest: manifest(adapter),
        }
    }

    fn nested_arrays(depth: usize) -> String {
        format!("{}{}", "[".repeat(depth), "]".repeat(depth))
    }

    #[test]
    fn structure_scan_ignores_brackets_commas_and_escapes_inside_strings() {
        let limits = IpcLimits::default();
        let scan = |text: &str| scan_structure(text, &limits).unwrap();
        // Brackets and commas inside strings are text, not structure.
        let quoted = scan(r#"{"a":"[[[{{,,,}}]]]","b":["x,y","[\"]"]}"#);
        assert_eq!(quoted.max_depth, 2);
        assert_eq!(quoted.elements, 5);
        // Escaped backslashes before a closing quote end the string.
        let escaped = scan(r#"["a\\",["b"]]"#);
        assert_eq!(escaped.max_depth, 2);
        assert_eq!(escaped.elements, 4);
        // An escaped quote does not end the string.
        let inner_quote = scan(r#"["a\"],[",1]"#);
        assert_eq!(inner_quote.max_depth, 1);
        assert_eq!(inner_quote.elements, 3);
        // Unterminated strings are left to the decoder to reject.
        assert_eq!(scan(r#"["abc"#).max_depth, 1);
        assert_eq!(scan(r#"["abc\"#).max_depth, 1);
    }

    #[test]
    fn too_many_elements_are_rejected_before_decoding() {
        let limits = IpcLimits::default();
        let many = format!("[{}1]", "1,".repeat(MAX_JSON_VALUES_TEST));
        assert!(matches!(
            decode_bounded::<serde_json::Value>(many.as_bytes(), &limits),
            Err(IpcError::TooManyValues { .. })
        ));
    }

    const MAX_JSON_VALUES_TEST: usize = super::super::limits::MAX_JSON_VALUES;

    #[test]
    fn invalid_utf8_is_rejected() {
        let limits = IpcLimits::default();
        let bytes = b"{\"a\":\"\xff\xfe\"}";
        assert_eq!(
            decode_bounded::<serde_json::Value>(bytes, &limits),
            Err(IpcError::InvalidUtf8 { valid_up_to: 6 })
        );
    }

    #[test]
    fn hello_identity_is_verified() {
        let limits = IpcLimits::default();
        let expected_adapter = adapter_id("nvml");
        let expected = HelloExpectation {
            adapter_id: &expected_adapter,
            generation: 3,
            nonce: "n0nce",
        };
        let good = serde_json::to_vec(&hello("nvml", 3, "n0nce")).unwrap();
        assert_eq!(
            decode_hello(&good, &limits, &expected).unwrap().generation,
            3
        );
        let cases = [
            (hello("hwmon", 3, "n0nce"), "adapter_id"),
            (hello("nvml", 2, "n0nce"), "generation"),
            (hello("nvml", 3, "other"), "nonce"),
        ];
        for (message, field) in cases {
            let bytes = serde_json::to_vec(&message).unwrap();
            assert_eq!(
                decode_hello(&bytes, &limits, &expected),
                Err(IpcError::HelloMismatch { field })
            );
        }
        let mut wrong_version = hello("nvml", 3, "n0nce");
        wrong_version.protocol_version = 2;
        let bytes = serde_json::to_vec(&wrong_version).unwrap();
        assert_eq!(
            decode_hello(&bytes, &limits, &expected),
            Err(IpcError::HelloMismatch {
                field: "protocol_version"
            })
        );
        let mut borrowed_manifest = hello("nvml", 3, "n0nce");
        borrowed_manifest.manifest = manifest("procfs");
        let bytes = serde_json::to_vec(&borrowed_manifest).unwrap();
        assert!(matches!(
            decode_hello(&bytes, &limits, &expected),
            Err(IpcError::HelloMismatch { .. })
        ));
        let mut invalid_manifest = hello("nvml", 3, "n0nce");
        invalid_manifest.manifest.max_inflight = 4;
        let bytes = serde_json::to_vec(&invalid_manifest).unwrap();
        assert!(matches!(
            decode_hello(&bytes, &limits, &expected),
            Err(IpcError::Contract { .. })
        ));
    }

    #[test]
    fn unknown_message_types_are_rejected() {
        let limits = IpcLimits::default();
        let mut value = serde_json::to_value(hello("nvml", 1, "n")).unwrap();
        value["type"] = serde_json::json!("helo");
        let bytes = serde_json::to_vec(&value).unwrap();
        let adapter = adapter_id("nvml");
        let expected = HelloExpectation {
            adapter_id: &adapter,
            generation: 1,
            nonce: "n",
        };
        assert!(matches!(
            decode_hello(&bytes, &limits, &expected),
            Err(IpcError::Malformed { .. })
        ));
        let request = sample_request(IpcRequestKind::Read, vec![1, 2]);
        let mut value = serde_json::to_value(&request).unwrap();
        for verb in ["kill", "signal", "execute"] {
            value["type"] = serde_json::json!(verb);
            let bytes = serde_json::to_vec(&value).unwrap();
            assert!(matches!(
                decode_request(&bytes, &limits),
                Err(IpcError::Malformed { .. })
            ));
        }
        let mut unknown_field = serde_json::to_value(&request).unwrap();
        unknown_field["path"] = serde_json::json!("/etc/shadow");
        let bytes = serde_json::to_vec(&unknown_field).unwrap();
        assert!(matches!(
            decode_request(&bytes, &limits),
            Err(IpcError::Malformed { .. })
        ));
    }

    fn sample_request(kind: IpcRequestKind, slots: Vec<u32>) -> IpcRequest {
        IpcRequest {
            protocol_version: 1,
            request_id: Uuid::new_v4(),
            adapter_id: adapter_id("hwmon"),
            host_generation: 1,
            kind,
            deadline_boot_ns: 42,
            catalog_revision: 1,
            config_revision: 1,
            cycle_id: 7,
            read_group: read_group("fast"),
            source_slots: slots,
        }
    }

    #[test]
    fn request_validation_rejects_slots_on_discover_and_too_many_slots() {
        let limits = IpcLimits::default();
        let good = serde_json::to_vec(&sample_request(IpcRequestKind::Read, vec![0, 1])).unwrap();
        assert_eq!(decode_request(&good, &limits).unwrap().source_slots, [0, 1]);
        let discover =
            serde_json::to_vec(&sample_request(IpcRequestKind::Discover, vec![1])).unwrap();
        assert!(matches!(
            decode_request(&discover, &limits),
            Err(IpcError::Contract { .. })
        ));
        let many =
            serde_json::to_vec(&sample_request(IpcRequestKind::Read, (0..257).collect())).unwrap();
        assert!(matches!(
            decode_request(&many, &limits),
            Err(IpcError::Contract { .. })
        ));
    }

    fn batch_with_records(count: usize) -> ObservationBatch {
        let records = (0..count)
            .map(|index| {
                ObservationBuilder::new(
                    metric_key("gpu0", "gpu.temperature", "device"),
                    "nvml",
                    &format!("nvml:gpu0:{index}"),
                )
                .f64(40.0)
                .build()
            })
            .collect();
        batch_of("nvml", "fast", 1, records)
    }

    #[test]
    fn more_than_256_records_are_rejected() {
        let limits = IpcLimits::default();
        let ok = serde_json::to_vec(&batch_with_records(256)).unwrap();
        assert_eq!(decode_batch(&ok, &limits).unwrap().records.len(), 256);
        let too_many = serde_json::to_vec(&batch_with_records(257)).unwrap();
        assert_eq!(
            decode_batch(&too_many, &limits),
            Err(IpcError::TooManyRecords {
                count: 257,
                limit: 256
            })
        );
        let tight = IpcLimits::new(1 << 20, 4 << 20, 8).unwrap();
        let nine = serde_json::to_vec(&batch_with_records(9)).unwrap();
        assert!(matches!(
            decode_batch(&nine, &tight),
            Err(IpcError::TooManyRecords { limit: 8, .. })
        ));
    }

    #[test]
    fn decimal_u64_overflow_in_a_batch_is_malformed() {
        let limits = IpcLimits::default();
        let mut value = serde_json::to_value(batch_with_records(1)).unwrap();
        value["cycle_id"] = serde_json::json!("18446744073709551616");
        let bytes = serde_json::to_vec(&value).unwrap();
        assert!(matches!(
            decode_batch(&bytes, &limits),
            Err(IpcError::Malformed { .. })
        ));
        let mut leading_zero = serde_json::to_value(batch_with_records(1)).unwrap();
        leading_zero["records"][0]["read_finished_boot_ns"] = serde_json::json!("01");
        let bytes = serde_json::to_vec(&leading_zero).unwrap();
        assert!(matches!(
            decode_batch(&bytes, &limits),
            Err(IpcError::Malformed { .. })
        ));
        let mut number_not_string = serde_json::to_value(batch_with_records(1)).unwrap();
        number_not_string["started_boot_ns"] = serde_json::json!(5);
        let bytes = serde_json::to_vec(&number_not_string).unwrap();
        assert!(matches!(
            decode_batch(&bytes, &limits),
            Err(IpcError::Malformed { .. })
        ));
    }

    #[test]
    fn decoded_budget_is_enforced() {
        // Short reason codes expand when decoded (24-byte String headers), so
        // the decoded estimate exceeds the wire size and a budget between the
        // two must reject the batch.
        let mut batch = batch_with_records(32);
        for record in &mut batch.records {
            record.reason_codes = (0..32).map(|code| format!("R{code}")).collect();
        }
        let bytes = serde_json::to_vec(&batch).unwrap();
        // Capacities after decoding differ from the hand-built fixture, so the
        // estimate is taken from what the decoder actually produces.
        let roomy = IpcLimits::new(bytes.len(), 4 << 20, 256).unwrap();
        let decoded = decode_batch(&bytes, &roomy).unwrap();
        let estimate = usize::try_from(decoded.decoded_bytes()).unwrap();
        assert!(estimate > bytes.len(), "fixture must expand when decoded");
        let tight = IpcLimits::new(bytes.len(), estimate - 1, 256).unwrap();
        assert!(matches!(
            decode_batch(&bytes, &tight),
            Err(IpcError::DecodedTooLarge { .. })
        ));
        let exact = IpcLimits::new(bytes.len(), estimate, 256).unwrap();
        assert!(decode_batch(&bytes, &exact).is_ok());
    }

    #[test]
    fn catalog_and_shutdown_replies_decode() {
        let limits = IpcLimits::default();
        let reply = CatalogReply {
            kind: CatalogTag::Catalog,
            request_id: Uuid::new_v4(),
            generation: 2,
            delta: CatalogDelta {
                complete: true,
                ..CatalogDelta::default()
            },
        };
        let bytes = serde_json::to_vec(&reply).unwrap();
        assert_eq!(decode_catalog(&bytes, &limits).unwrap(), reply);
        let mut invalid = reply.clone();
        invalid.delta.next_page = Some(1);
        let bytes = serde_json::to_vec(&invalid).unwrap();
        assert!(matches!(
            decode_catalog(&bytes, &limits),
            Err(IpcError::Contract { .. })
        ));
        let ack = ShutdownAck {
            kind: ShutdownTag::ShutdownAck,
            request_id: Uuid::new_v4(),
        };
        let bytes = serde_json::to_vec(&ack).unwrap();
        assert_eq!(decode_shutdown_ack(&bytes, &limits).unwrap(), ack);
        // A batch is not a shutdown ack.
        let batch = serde_json::to_vec(&batch_with_records(1)).unwrap();
        assert!(decode_shutdown_ack(&batch, &limits).is_err());
    }

    #[test]
    fn spec_examples_decode() {
        let limits = IpcLimits::default();
        let request = include_bytes!("../../../../contracts/examples/ipc-request.read.json");
        assert_eq!(
            decode_request(request, &limits).unwrap().source_slots,
            [0, 1]
        );
        let batch = include_bytes!("../../../../contracts/examples/observation-batch.partial.json");
        let decoded = decode_batch(batch, &limits).unwrap();
        assert_eq!(decoded.status, BatchStatus::Partial);
    }

    proptest! {
        #[test]
        fn arbitrary_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
            let limits = IpcLimits::default();
            let _ = decode_bounded::<serde_json::Value>(&bytes, &limits);
            let _ = decode_batch(&bytes, &limits);
            let _ = decode_request(&bytes, &limits);
        }
    }

    /// Every depth up to 63, so 32 (accepted) and 33 (rejected) always run.
    #[test]
    fn depth_limit_is_exact() {
        let limits = IpcLimits::default();
        for depth in 1usize..64 {
            let result =
                decode_bounded::<serde_json::Value>(nested_arrays(depth).as_bytes(), &limits);
            assert_eq!(result.is_ok(), depth <= 32, "depth {depth}");
        }
    }
}
