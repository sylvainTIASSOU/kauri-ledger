use kauri_ledger_store::idempotency::{RequestHashError, compute_request_hash};

const METHOD: &str = "POST";
const PATH: &str = "/v1/ledgers/test/transactions";

fn hash(body: &[u8]) -> [u8; 32] {
    compute_request_hash(METHOD, PATH, body).unwrap()
}

#[test]
fn equivalent_json_produces_identical_hashes() {
    let original = br#"{"amount":100,"metadata":{"b":2,"a":1}}"#;

    let reordered = br#"{
        "metadata": {
            "a": 1,
            "b": 2
        },
        "amount": 100
    }"#;

    assert_eq!(hash(original), hash(reordered));
}

#[test]
fn canonicalization_preserves_nested_array_order() {
    let original = br#"{"items":[{"b":2,"a":1},3]}"#;
    let reordered_properties = br#"{"items":[{"a":1,"b":2},3]}"#;
    let reordered_array = br#"{"items":[3,{"a":1,"b":2}]}"#;

    assert_eq!(hash(original), hash(reordered_properties));
    assert_ne!(hash(original), hash(reordered_array));
}

#[test]
fn hash_is_scoped_to_method_and_path() {
    let body = br#"{"amount":100}"#;

    let post_hash = compute_request_hash("POST", "/v1/ledgers/test/transactions", body).unwrap();

    let get_hash = compute_request_hash("GET", "/v1/ledgers/test/transactions", body).unwrap();

    let reverse_hash =
        compute_request_hash("POST", "/v1/ledgers/test/transactions/reverse", body).unwrap();

    assert_ne!(post_hash, get_hash);
    assert_ne!(post_hash, reverse_hash);
    assert_ne!(get_hash, reverse_hash);
}

#[test]
fn different_payloads_produce_different_hashes() {
    let first = br#"{"amount":100}"#;
    let second = br#"{"amount":200}"#;

    assert_ne!(hash(first), hash(second));
}

#[test]
fn rejects_duplicate_keys_and_malformed_json() {
    let invalid_bodies: [&[u8]; 5] = [
        br#"{"amount":100,"amount":200}"#,
        br#"{"nested":{"value":1,"value":2}}"#,
        br#"{"broken":"#,
        b"not-json",
        b"\xff",
    ];

    for body in invalid_bodies {
        let error =
            compute_request_hash(METHOD, PATH, body).expect_err("Invalid JSON must not be hashed");

        assert!(
            matches!(error, RequestHashError::InvalidJson(_)),
            "Unexpected error: {error:?}"
        );
    }
}

#[test]
fn rejects_unsafe_json_integers() {
    let invalid_bodies: [&[u8]; 2] = [
        br#"{"value":9007199254740993}"#,
        br#"{"value":-9007199254740993}"#,
    ];

    for body in invalid_bodies {
        let error =
            compute_request_hash(METHOD, PATH, body).expect_err("Unsafe integer must be rejected");

        assert!(matches!(error, RequestHashError::InvalidJson(_)));
    }
}

#[test]
fn hashes_the_received_body_before_server_defaults() {
    let received = br#"{"amount":100}"#;

    let with_server_default = br#"{"amount":100,"effective_at":"2026-11-23T10:00:00Z"}"#;

    assert_ne!(hash(received), hash(with_server_default));
}

#[test]
fn matches_a_fixed_sha256_test_vector() {
    let actual = hash(br#"{"b":2,"a":1}"#);

    assert_eq!(
        hex::encode(actual),
        "b493f7faf9366d62cf33fbfa80ae59ac88d2134496cf30effa307160d4354429"
    );
}
