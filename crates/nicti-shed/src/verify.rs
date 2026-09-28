//! Verifies a downloaded installer's minisign signature against a compiled-in public key. This
//! is the actual trust boundary #249/ADR-0249 relies on -- see that ADR for why this holds
//! independently of Authenticode/SignPath signing.

use minisign_verify::{PublicKey, Signature};

use crate::ShedError;

/// Verifies `installer_bytes` against `signature_text` (the `.minisig` sidecar's own file
/// contents, unmodified) using `public_key_base64` (the key's base64 line only -- not the
/// `untrusted comment:` header line minisign key files also carry).
///
/// Deliberately collapses every failure mode (a malformed key, a malformed signature, a
/// signature that doesn't match) into one [`ShedError::Verify`] with no further detail -- see
/// that variant's own doc comment for why.
pub fn verify_installer(
    public_key_base64: &str,
    installer_bytes: &[u8],
    signature_text: &str,
) -> Result<(), ShedError> {
    let public_key = PublicKey::from_base64(public_key_base64).map_err(|_| ShedError::Verify)?;
    let signature = Signature::decode(signature_text).map_err(|_| ShedError::Verify)?;
    public_key
        .verify(installer_bytes, &signature, false)
        .map_err(|_| ShedError::Verify)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A real minisign keypair/signature triple, generated solely to exercise this test -- see
    // `crate::PUBLIC_KEY_BASE64`'s own doc comment. `FIXTURE_BYTES` are the exact bytes that
    // were signed; `FIXTURE_SIGNATURE` is the resulting `.minisig` sidecar's own file contents,
    // verbatim.
    const FIXTURE_BYTES: &[u8] = b"fake-installer-bytes-for-test-fixture\n";
    // The trusted-comment line is itself covered by a second signature (minisig's own
    // "global signature", the final base64 blob) -- it must be reproduced byte-for-byte from
    // the original `rsign sign` output or verification fails on the trusted comment's own
    // signature, not just the main one.
    const FIXTURE_SIGNATURE: &str = "untrusted comment: signature from rsign secret key\n\
RUQQhGAWQ6j6RtvjYiw8w7zIOsS1V0f1435fVZ06i30IpDC6sI+m3vbVmkty3PlekHnLcRlyQUpgjD6/qDCSNTPRGjSuFfKSnAs=\n\
trusted comment: nicti-shed test fixture\n\
rVaUX7HkjX8VsxZT+LJoW0ebkb4nh2MWu2ox0+ZM90StlJQ3bCO9f5UFSCubcEUJ6S00IDtPoLeYabeDst3qCA==\n";
    // An unrelated keypair's public key -- proves a signature valid under the real key is
    // rejected when checked against the wrong one, not just when the signature is absent.
    const WRONG_PUBLIC_KEY_BASE64: &str =
        "RWQ/avLbiqQkor7DX228MbPTOq9ZIMc4zwRMqGZ1Dv1dcoQI2bJRgE+W";

    #[test]
    fn accepts_a_genuine_signature() {
        verify_installer(crate::PUBLIC_KEY_BASE64, FIXTURE_BYTES, FIXTURE_SIGNATURE)
            .expect("a real signature over its own real bytes must verify");
    }

    #[test]
    fn rejects_tampered_bytes() {
        let mut tampered = FIXTURE_BYTES.to_vec();
        tampered[0] ^= 0xFF;
        let result = verify_installer(crate::PUBLIC_KEY_BASE64, &tampered, FIXTURE_SIGNATURE);
        assert!(
            matches!(result, Err(ShedError::Verify)),
            "a signature must never verify against bytes it wasn't signed over"
        );
    }

    #[test]
    fn rejects_a_wrong_public_key() {
        let result = verify_installer(WRONG_PUBLIC_KEY_BASE64, FIXTURE_BYTES, FIXTURE_SIGNATURE);
        assert!(
            matches!(result, Err(ShedError::Verify)),
            "a genuine signature from one key must not verify under a different key"
        );
    }

    #[test]
    fn rejects_a_malformed_signature() {
        let result = verify_installer(crate::PUBLIC_KEY_BASE64, FIXTURE_BYTES, "not a signature");
        assert!(matches!(result, Err(ShedError::Verify)));
    }

    #[test]
    fn rejects_a_malformed_public_key() {
        let result = verify_installer("not a key", FIXTURE_BYTES, FIXTURE_SIGNATURE);
        assert!(matches!(result, Err(ShedError::Verify)));
    }
}
