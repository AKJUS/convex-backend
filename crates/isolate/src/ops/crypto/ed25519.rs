// Copyright 2018-2023 the Deno authors. All rights reserved. MIT license.
// https://github.com/denoland/deno/blob/main/ext/crypto/ed25519.rs

use deno_core::{
    JsBuffer,
    ToJsBuffer,
};
use elliptic_curve::pkcs8::PrivateKeyInfo;
use p256::pkcs8::der::Decode as _;
use ring::signature::Ed25519KeyPair;
use spki::{
    der::{
        AnyRef,
        Decode,
    },
    SubjectPublicKeyInfo,
};

use super::CryptoOps;

// id-Ed25519 OBJECT IDENTIFIER ::= { 1 3 101 112 }
pub const ED25519_OID: const_oid::ObjectIdentifier =
    const_oid::ObjectIdentifier::new_unwrap("1.3.101.112");

impl CryptoOps {
    pub fn sign_ed25519(key: &[u8], data: &[u8]) -> Option<ToJsBuffer> {
        let pair = match Ed25519KeyPair::from_seed_unchecked(key) {
            Ok(p) => p,
            Err(_) => return None,
        };
        Some(pair.sign(data).as_ref().to_vec().into())
    }

    pub fn verify_ed25519(pubkey: &[u8], data: &[u8], signature: &[u8]) -> bool {
        ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, pubkey)
            .verify(data, signature)
            .is_ok()
    }

    pub fn import_spki_ed25519(key_data: JsBuffer) -> Option<ToJsBuffer> {
        // 2-3.
        let pk_info: SubjectPublicKeyInfo<AnyRef, Vec<u8>> =
            match spki::SubjectPublicKeyInfo::from_der(&key_data) {
                Ok(pk_info) => pk_info,
                Err(_) => return None,
            };
        // 4.
        let alg = pk_info.algorithm.oid;
        if alg != ED25519_OID {
            return None;
        }
        // 5.
        if pk_info.algorithm.parameters.is_some() {
            return None;
        }
        Some(pk_info.subject_public_key.into())
    }

    pub fn import_pkcs8_ed25519(key_data: JsBuffer) -> Option<ToJsBuffer> {
        // 2-3.
        // This should probably use OneAsymmetricKey instead
        let pk_info = match PrivateKeyInfo::from_der(&key_data) {
            Ok(pk_info) => pk_info,
            Err(_) => return None,
        };
        // 4.
        let alg = pk_info.algorithm.oid;
        if alg != ED25519_OID {
            return None;
        }
        // 5.
        if pk_info.algorithm.parameters.is_some() {
            return None;
        }
        // 6.
        // CurvePrivateKey ::= OCTET STRING
        if pk_info.private_key.len() != 34 {
            return None;
        }
        Some(pk_info.private_key[2..].to_vec().into())
    }
}
