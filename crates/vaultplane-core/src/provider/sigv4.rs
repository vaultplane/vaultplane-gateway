// Copyright 2026 VaultPlane Contributors
// SPDX-License-Identifier: Apache-2.0

//! Minimal AWS Signature Version 4 signing for the Bedrock connector.
//!
//! Implements the documented SigV4 algorithm using the vetted `hmac` and `sha2`
//! crates (no AWS SDK dependency). The signing-key derivation is unit-tested against
//! AWS's published example vector.

use std::time::{SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts a key of any length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

/// Derive the SigV4 signing key via the documented HMAC chain.
pub(crate) fn signing_key(secret: &str, date_stamp: &str, region: &str, service: &str) -> Vec<u8> {
    let k_date = hmac_sha256(format!("AWS4{secret}").as_bytes(), date_stamp.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    hmac_sha256(&k_service, b"aws4_request")
}

/// AWS-style percent-encoding for a URI path segment (RFC 3986 unreserved unescaped).
pub(crate) fn uri_encode_segment(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for &byte in input.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// The current time as (`amz_date` = `YYYYMMDDTHHMMSSZ`, `date_stamp` = `YYYYMMDD`),
/// in UTC.
pub(crate) fn now_amz_date() -> (String, String) {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_amz_date(secs)
}

fn format_amz_date(secs: u64) -> (String, String) {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (year, month, day) = civil_from_days(days);
    (
        format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z"),
        format!("{year:04}{month:02}{day:02}"),
    )
}

/// Convert a count of days since 1970-01-01 into a civil (year, month, day) date.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// Sign a POST request and return the `Authorization` header value.
///
/// `canonical_path` is the already-encoded request path. The caller must send the
/// same `content-type`, `host`, `x-amz-date`, and (when present) `x-amz-security-token`
/// headers that are signed here.
#[allow(clippy::too_many_arguments)]
pub(crate) fn sign_post(
    access_key: &str,
    secret_key: &str,
    session_token: Option<&str>,
    region: &str,
    service: &str,
    host: &str,
    canonical_path: &str,
    content_type: &str,
    body: &[u8],
    amz_date: &str,
    date_stamp: &str,
) -> String {
    let payload_hash = sha256_hex(body);

    // Canonical headers are sorted by lowercase header name.
    let mut canonical_headers =
        format!("content-type:{content_type}\nhost:{host}\nx-amz-date:{amz_date}\n");
    let mut signed_headers = String::from("content-type;host;x-amz-date");
    if let Some(token) = session_token {
        canonical_headers.push_str(&format!("x-amz-security-token:{token}\n"));
        signed_headers.push_str(";x-amz-security-token");
    }

    let canonical_request =
        format!("POST\n{canonical_path}\n\n{canonical_headers}\n{signed_headers}\n{payload_hash}");

    let scope = format!("{date_stamp}/{region}/{service}/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );

    let key = signing_key(secret_key, date_stamp, region, service);
    let signature = hex::encode(hmac_sha256(&key, string_to_sign.as_bytes()));

    format!(
        "AWS4-HMAC-SHA256 Credential={access_key}/{scope}, SignedHeaders={signed_headers}, Signature={signature}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signing_key_matches_aws_example_vector() {
        // From the AWS "deriving a signing key for Signature Version 4" example.
        let key = signing_key(
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            "20120215",
            "us-east-1",
            "iam",
        );
        assert_eq!(
            hex::encode(key),
            "f4780e2d9f65fa895f9c67b32ce1baf0b0d8a43505a000a1a9e090d414db404d"
        );
    }

    #[test]
    fn formats_dates_in_utc() {
        assert_eq!(
            format_amz_date(0),
            ("19700101T000000Z".to_string(), "19700101".to_string())
        );
        assert_eq!(
            format_amz_date(1_700_000_000),
            ("20231114T221320Z".to_string(), "20231114".to_string())
        );
    }

    #[test]
    fn uri_encodes_reserved_characters() {
        assert_eq!(
            uri_encode_segment("anthropic.claude-3-7-sonnet:0"),
            "anthropic.claude-3-7-sonnet%3A0"
        );
    }

    #[test]
    fn sign_post_produces_an_authorization_header() {
        let authorization = sign_post(
            "AKIDEXAMPLE",
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            None,
            "us-east-1",
            "bedrock",
            "bedrock-runtime.us-east-1.amazonaws.com",
            "/model/m/invoke",
            "application/json",
            b"{}",
            "20150830T123600Z",
            "20150830",
        );
        assert!(authorization.starts_with(
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/bedrock/aws4_request, "
        ));
        assert!(authorization.contains("SignedHeaders=content-type;host;x-amz-date,"));
        assert!(authorization.contains("Signature="));
    }
}
