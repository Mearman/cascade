//! AWS Signature Version 4 request signing.
//!
//! Implements the `SigV4` signing algorithm for S3-compatible APIs.
//! Reference: <https://docs.aws.amazon.com/general/latest/gr/sigv4-create-canonical-request.html>

use std::fmt::Write as _;

use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

/// Compute SHA-256 hex digest of the given bytes.
pub(crate) fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

/// Compute HMAC-SHA256 of `data` using `key`.
///
/// `Hmac<Sha256>::new_from_slice` only fails for zero-length keys. The callers
/// in this module always pass non-empty byte slices, so the error branch is
/// unreachable. If it were ever reached the all-zero sentinel would produce an
/// invalid signature, causing the request to fail authentication rather than
/// panicking — a safe degradation.
fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    HmacSha256::new_from_slice(key).map_or_else(
        |_| vec![0u8; 32],
        |mut mac| {
            mac.update(data);
            mac.finalize().into_bytes().to_vec()
        },
    )
}

/// Derive the `SigV4` signing key from the secret access key, date, region, and service.
pub(crate) fn derive_signing_key(
    secret_access_key: &str,
    date: &str,
    region: &str,
    service: &str,
) -> Vec<u8> {
    let k_date = hmac_sha256(
        format!("AWS4{secret_access_key}").as_bytes(),
        date.as_bytes(),
    );
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    hmac_sha256(&k_service, b"aws4_request")
}

/// URI-encode a string component (path segment or query value).
///
/// Encodes all characters except unreserved ones: `A-Z a-z 0-9 - _ . ~`.
/// If `encode_slash` is `true`, forward slashes are also encoded.
#[must_use]
pub(crate) fn uri_encode(value: &str, encode_slash: bool) -> String {
    let mut encoded = String::with_capacity(value.len() * 3);
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char);
            }
            b'/' if !encode_slash => {
                encoded.push('/');
            }
            other => {
                encoded.push('%');
                // Two hex digits; write! into String never fails.
                write!(encoded, "{other:02X}").unwrap_or(());
            }
        }
    }
    encoded
}

/// Parameters required to sign a request.
#[derive(Debug)]
pub(crate) struct SigningParams<'a> {
    pub method: &'a str,
    pub uri_path: &'a str,
    pub query_string: &'a str,
    pub host: &'a str,
    pub payload_hash: &'a str,
    pub access_key_id: &'a str,
    pub secret_access_key: &'a str,
    pub region: &'a str,
    pub service: &'a str,
    pub now: DateTime<Utc>,
}

/// Computed `SigV4` headers to add to the request.
#[derive(Debug)]
pub(crate) struct SignedHeaders {
    pub x_amz_date: String,
    pub x_amz_content_sha256: String,
    pub authorization: String,
}

/// Sign a request using AWS Signature Version 4.
///
/// Returns the headers that must be added to the outgoing request.
pub(crate) fn sign(params: &SigningParams<'_>) -> SignedHeaders {
    let datetime = params.now.format("%Y%m%dT%H%M%SZ").to_string();
    let date = params.now.format("%Y%m%d").to_string();

    // Canonical URI: URI-encode the path, preserving slashes.
    let canonical_uri = uri_encode(params.uri_path, false);

    // Canonical query string: percent-encode and sort by key then value.
    let canonical_querystring = build_canonical_query_string(params.query_string);

    // Canonical headers (must be lowercase, trimmed, sorted).
    let canonical_headers = format!(
        "host:{}\nx-amz-content-sha256:{}\nx-amz-date:{}\n",
        params.host, params.payload_hash, datetime
    );
    let signed_headers = "host;x-amz-content-sha256;x-amz-date";

    // Canonical request.
    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        params.method,
        canonical_uri,
        canonical_querystring,
        canonical_headers,
        signed_headers,
        params.payload_hash,
    );

    // String to sign.
    let credential_scope = format!("{date}/{}/{}/aws4_request", params.region, params.service);
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{}\n{}",
        datetime,
        credential_scope,
        sha256_hex(canonical_request.as_bytes()),
    );

    // Signing key and signature.
    let signing_key = derive_signing_key(
        params.secret_access_key,
        &date,
        params.region,
        params.service,
    );
    let signature = hex::encode(hmac_sha256(&signing_key, string_to_sign.as_bytes()));

    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={}/{},SignedHeaders={},Signature={}",
        params.access_key_id, credential_scope, signed_headers, signature
    );

    SignedHeaders {
        x_amz_date: datetime,
        x_amz_content_sha256: params.payload_hash.to_string(),
        authorization,
    }
}

/// Sort and encode query string parameters into the canonical form required by `SigV4`.
fn build_canonical_query_string(query_string: &str) -> String {
    if query_string.is_empty() {
        return String::new();
    }

    let mut pairs: Vec<(String, String)> = query_string
        .split('&')
        .filter(|s| !s.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (uri_encode(k, true), uri_encode(v, true))
        })
        .collect();

    pairs.sort_unstable();

    pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_encode_unreserved_chars_unchanged() {
        assert_eq!(uri_encode("abc-123._~", true), "abc-123._~");
    }

    #[test]
    fn uri_encode_slash_when_requested() {
        assert_eq!(uri_encode("a/b", true), "a%2Fb");
    }

    #[test]
    fn uri_encode_slash_preserved_when_not_encoding() {
        assert_eq!(uri_encode("a/b", false), "a/b");
    }

    #[test]
    fn uri_encode_special_chars() {
        assert_eq!(uri_encode("hello world", true), "hello%20world");
        assert_eq!(uri_encode("a=b", true), "a%3Db");
    }

    #[test]
    fn canonical_query_string_sorted() {
        let qs = "b=2&a=1";
        assert_eq!(build_canonical_query_string(qs), "a=1&b=2");
    }

    #[test]
    fn canonical_query_string_empty() {
        assert_eq!(build_canonical_query_string(""), "");
    }

    #[test]
    fn sha256_hex_known_value() {
        // SHA-256 of empty string.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
