// alot of tests are gone here because they depend on the database, let me know within two weeks if
// you would like more
use api::server::utils::signature_utils::SignatureUtil;

#[test]
fn test_signature_generation() {
    let util = SignatureUtil::new("test_secret".to_string());
    let sig1 = util.generate_signature("client123", 1234567890, "https://example.com");
    let sig2 = util.generate_signature("client123", 1234567890, "https://example.com");

    assert_eq!(sig1, sig2);
}

#[test]
fn test_signature_verification() {
    let util = SignatureUtil::new("test_secret".to_string());
    let future_expiry = SignatureUtil::generate_expiry(12);
    let url = "https://example.com";
    let client_id = "client123";

    let signature = util.generate_signature(client_id, future_expiry, url);

    // valid signature should verify
    assert!(util.verify_signature(client_id, future_expiry, url, &signature));

    // invalid signature should fail
    assert!(!util.verify_signature(client_id, future_expiry, url, "invalid"));

    // different client should fail
    assert!(!util.verify_signature("different_client", future_expiry, url, &signature));
}

#[test]
fn test_expired_signature() {
    let util = SignatureUtil::new("test_secret".to_string());
    let past_expiry = 1234567890; // a while ago
    let url = "https://example.com";
    let client_id = "client123";

    let signature = util.generate_signature(client_id, past_expiry, url);

    // expired signature should fail even if signature is correct
    assert!(!util.verify_signature(client_id, past_expiry, url, &signature));
}
