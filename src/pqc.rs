//! Post-quantum signatures over EffectCerts (FIPS 204 / ML-DSA-65).
//!
//! An [`EffectCert`](crate::fence::EffectCert) is *content-addressed*: its
//! `hash` is a SHA-256 digest of its fields, so any edit is DETECTABLE by
//! recomputing the digest. That is tamper-EVIDENCE, and it is enough to catch
//! accidental corruption or a naive edit.
//!
//! It is NOT tamper-PROOF: anyone can mint a fresh cert with a matching hash,
//! because computing SHA-256 needs no secret. For a regulated audit trail the
//! question is not only "was this record altered?" but "did the party that
//! holds the signing key actually attest to it?" — a claim a hash cannot make.
//!
//! This module answers it by signing the cert's hash with ML-DSA-65, the
//! NIST-standardised (FIPS 204, 2024) lattice signature. Lattice signatures
//! are believed secure against both classical and quantum adversaries, so an
//! attester's certificates cannot be forged even by someone with a
//! cryptographically relevant quantum computer — the property a long-lived
//! compliance record (SEC 17a-4 retention runs to years) actually needs.
//!
//! What this does and does not do, stated plainly:
//! * It signs the cert's SHA-256 hash, so the signature transitively covers
//!   every field the hash covers. It does not re-hash the fields itself.
//! * It proves *who attested*, not *what happened*. A signed cert for a
//!   fabricated action is still a fabricated action, signed. Signing raises
//!   forgery-resistance; it does not make the underlying claim true.
//! * The key lives wherever the caller puts it. This module does not manage
//!   key storage, rotation, or an HSM — those are deployment concerns.
//!
//! Enabled by the `pqc` feature; the base crate does not pull in the lattice
//! implementation.

use fips204::ml_dsa_65;
use fips204::traits::{KeyGen, SerDes, Signer, Verifier};

use crate::fence::EffectCert;

/// A keypair for signing EffectCerts. The private key never leaves this
/// struct; export only the public key (via [`public_key_bytes`]) for
/// verifiers.
///
/// [`public_key_bytes`]: CertSigner::public_key_bytes
pub struct CertSigner {
    sk: ml_dsa_65::PrivateKey,
    pk: ml_dsa_65::PublicKey,
}

/// A detached ML-DSA-65 signature over an [`EffectCert`]'s hash.
#[derive(Clone, PartialEq, Eq)]
pub struct CertSignature(pub Vec<u8>);

impl CertSigner {
    /// Generate a fresh signing keypair. Fails only if the platform RNG is
    /// unavailable.
    pub fn generate() -> Result<Self, &'static str> {
        let (pk, sk) = ml_dsa_65::try_keygen()?;
        Ok(Self { sk, pk })
    }

    /// The public key bytes a verifier needs. Safe to publish; the private
    /// key cannot be derived from it.
    pub fn public_key_bytes(&self) -> Vec<u8> {
        self.pk.clone().into_bytes().to_vec()
    }

    /// Sign a cert's content address. The signature covers `cert.hash`,
    /// which in turn covers every field of the cert (see module docs). An
    /// empty context string is used — the hash is already domain-separated
    /// by being a cert digest.
    pub fn sign(&self, cert: &EffectCert) -> Result<CertSignature, &'static str> {
        let sig = self.sk.try_sign(cert.hash.as_bytes(), &[])?;
        Ok(CertSignature(sig.to_vec()))
    }
}

/// Verify a signature against a cert and a public key, WITHOUT trusting the
/// signer. Returns `true` only if the cert's own hash is internally valid
/// AND the signature is a genuine ML-DSA-65 signature of that hash under
/// `public_key_bytes`.
///
/// The internal-hash check matters: a signature over a hash that does not
/// match the cert's fields would verify cryptographically while attesting to
/// a cert whose content had been swapped. Both gates must pass.
pub fn verify(cert: &EffectCert, sig: &CertSignature, public_key_bytes: &[u8]) -> bool {
    if !cert.verify() {
        return false; // the cert's hash does not match its own fields
    }
    let pk_arr: [u8; ml_dsa_65::PK_LEN] = match public_key_bytes.try_into() {
        Ok(a) => a,
        Err(_) => return false, // wrong public-key length
    };
    let pk = match ml_dsa_65::PublicKey::try_from_bytes(pk_arr) {
        Ok(pk) => pk,
        Err(_) => return false,
    };
    let sig_arr: [u8; ml_dsa_65::SIG_LEN] = match sig.0.as_slice().try_into() {
        Ok(a) => a,
        Err(_) => return false, // wrong signature length
    };
    pk.verify(cert.hash.as_bytes(), &sig_arr, &[])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fence::{EffectFence, EffectRequest, VectorClock, Admission,
                       prepare_effect_fence, commit_effect_cert};
    use serde_json::json;

    fn one_cert() -> EffectCert {
        let fence = EffectFence::new();
        let req = EffectRequest {
            intent: "charge:order-1".into(),
            parent: None,
            domain: "order:1".into(),
            tool: "charge_card".into(),
            args: json!({ "amount": 4900 }),
            read_set: vec![],
            agent: "agent-A".into(),
            known_clock: VectorClock::new(),
        };
        match prepare_effect_fence(&fence, req).unwrap() {
            Admission::Fresh(p) => commit_effect_cert(&fence, p, json!({"ok": true})).unwrap(),
            _ => panic!("expected fresh"),
        }
    }

    #[test]
    fn genuine_signature_verifies() {
        let signer = CertSigner::generate().unwrap();
        let cert = one_cert();
        let sig = signer.sign(&cert).unwrap();
        assert!(verify(&cert, &sig, &signer.public_key_bytes()));
    }

    #[test]
    fn a_different_key_does_not_verify() {
        let signer = CertSigner::generate().unwrap();
        let attacker = CertSigner::generate().unwrap();
        let cert = one_cert();
        let sig = signer.sign(&cert).unwrap();
        // Same cert, same signature, WRONG public key -> must fail.
        assert!(!verify(&cert, &sig, &attacker.public_key_bytes()));
    }

    #[test]
    fn tampering_with_the_cert_breaks_verification() {
        let signer = CertSigner::generate().unwrap();
        let mut cert = one_cert();
        let sig = signer.sign(&cert).unwrap();
        // Swap the recorded result; the stored hash no longer matches the
        // fields, so verify() must reject even though the signature is real.
        cert.result = json!({ "ok": false });
        assert!(!verify(&cert, &sig, &signer.public_key_bytes()));
    }

    #[test]
    fn a_garbage_signature_does_not_verify() {
        let signer = CertSigner::generate().unwrap();
        let cert = one_cert();
        let bogus = CertSignature(vec![0u8; 3309]); // right length, wrong bytes
        assert!(!verify(&cert, &bogus, &signer.public_key_bytes()));
    }
}
