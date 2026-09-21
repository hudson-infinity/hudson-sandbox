#![allow(clippy::unwrap_used)]
use prost::Message;
use sandbox_protocol::{
    AllocationId, Id, OperationId, file_wire as wire, files as m, guest as w, guest_model::Context,
};
fn upload() -> m::Upload {
    m::Upload {
        operation_id: OperationId::generate(),
        path: "dir/file".into(),
        size: 10,
        sha256: [4; 32],
        mode: 0o644,
    }
}
#[test]
fn file_receipts_round_trip_and_reject_incomplete_or_forged_shapes() {
    let input = upload();
    let receipt = m::Receipt {
        version: 1,
        context: Context {
            allocation_id: AllocationId::generate(),
            generation: 1,
            boot_id: "boot".into(),
        },
        digest: input.digest().unwrap(),
        upload: input,
        state: m::State::Committed,
    };
    let original: w::FileReceipt = (&receipt).into();
    assert_eq!(
        m::Receipt::try_from(w::FileReceipt::decode(original.encode_to_vec().as_slice()).unwrap())
            .unwrap(),
        receipt
    );
    for mode in 0..9 {
        let mut v = original.clone();
        match mode {
            0 => v.state = 0,
            1 => v.state = 999,
            2 => v.version = 2,
            3 => v.context = None,
            4 => v.upload = None,
            5 => v.digest.clear(),
            6 => v.upload.as_mut().unwrap().path = "../escape".into(),
            7 => v.upload.as_mut().unwrap().size = m::MAX_FILE_BYTES + 1,
            _ => v.upload.as_mut().unwrap().mode = 0o4755,
        }
        assert!(m::Receipt::try_from(v).is_err(), "mode {mode}");
    }
}
#[test]
fn file_chunks_fit_existing_frames_and_debug_omits_bytes() {
    let write = w::WriteFile {
        operation: Some(wire::operation(&upload()).unwrap()),
        offset: 0,
        data: vec![42; m::MAX_CHUNK_BYTES],
    };
    wire::validate_write(&write).unwrap();
    let request = w::Request {
        version: 1,
        request_id: OperationId::generate().to_string(),
        context: None,
        action: Some(w::request::Action::WriteFile(write.clone())),
    };
    assert!(request.encoded_len() < sandbox_protocol::guest_wire::MAX_FRAME);
    assert!(!format!("{request:?}").contains("42, 42"));
    let chunk = w::FileChunk {
        data: b"secret-payload".to_vec(),
        ..Default::default()
    };
    assert!(!format!("{chunk:?}").contains("secret-payload"));
    let mut bad = write.clone();
    bad.offset = u64::MAX;
    assert!(wire::validate_write(&bad).is_err());
    bad = write.clone();
    bad.data.push(0);
    assert!(wire::validate_write(&bad).is_err());
    bad = write.clone();
    bad.operation = None;
    assert!(wire::validate_write(&bad).is_err());
    bad = write;
    bad.operation.as_mut().unwrap().digest.clear();
    assert!(wire::validate_write(&bad).is_err());
}
#[test]
fn capture_and_range_bounds_reject_bad_ids_digests_and_offsets() {
    let good = w::FileCapture {
        capture_id: OperationId::generate().to_string(),
        path: "file".into(),
        size: m::MAX_FILE_BYTES,
        sha256: vec![0; 32],
        expires_unix_ms: 1,
    };
    wire::validate_capture(&good).unwrap();
    for mode in 0..5 {
        let mut v = good.clone();
        match mode {
            0 => v.capture_id = "invalid".into(),
            1 => v.path = "/absolute".into(),
            2 => v.size += 1,
            3 => v.sha256.clear(),
            _ => v.expires_unix_ms = 0,
        }
        assert!(wire::validate_capture(&v).is_err());
    }
    let mut read = w::ReadFile {
        handle: Some(wire::handle(&good).unwrap()),
        offset: 0,
        limit: m::MAX_CHUNK_BYTES as u32,
    };
    wire::validate_read(&read).unwrap();
    read.limit = 0;
    assert!(wire::validate_read(&read).is_err());
    read.limit = 1;
    read.offset = u64::MAX;
    assert!(wire::validate_read(&read).is_err());
}
