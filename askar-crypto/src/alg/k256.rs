//! Elliptic curve ECDH and ECDSA support on curve secp256k1

use core::convert::{TryFrom, TryInto};

use k256::{
    ecdsa::{
        signature::{Signer, Verifier},
        Signature, SigningKey, VerifyingKey,
    },
    elliptic_curve::{self, ecdh::diffie_hellman, sec1::Coordinates},
    EncodedPoint, PublicKey, SecretKey,
};
use subtle::ConstantTimeEq;

use super::{EcCurves, HasKeyAlg, KeyAlg};
use crate::{
    buffer::{ArrayKey, WriteBuffer},
    error::Error,
    generic_array::typenum::{U32, U33, U65},
    jwk::{FromJwk, JwkEncoder, JwkParts, ToJwk},
    kdf::KeyExchange,
    random::KeyMaterial,
    repr::{KeyGen, KeyMeta, KeyPublicBytes, KeySecretBytes, KeypairBytes, KeypairMeta},
    sign::{KeySigVerify, KeySign, SignatureType},
};

// SECURITY: PublicKey contains a k256::AffinePoint, which is always checked
// to be on the curve when loaded:
// <https://github.com/RustCrypto/elliptic-curves/blob/a38df18d221a4ca27851c4523f90ceded6bbd361/k256/src/arithmetic/affine.rs#L96>
// The identity point is rejected when converting into a k256::PublicKey.
// This satisfies 5.6.2.3.4 ECC Partial Public-Key Validation Routine from
// NIST SP 800-56A: _Recommendation for Pair-Wise Key-Establishment Schemes
// Using Discrete Logarithm Cryptography_.

/// The length of an ES256K signature
pub const ES256K_SIGNATURE_LENGTH: usize = 64;

/// The length of a compressed public key in bytes
pub const PUBLIC_KEY_LENGTH: usize = 33;
/// The length of a secret key
pub const SECRET_KEY_LENGTH: usize = 32;
/// The length of a keypair in bytes
pub const KEYPAIR_LENGTH: usize = SECRET_KEY_LENGTH + PUBLIC_KEY_LENGTH;

/// The 'kty' value of an elliptic curve key JWK
pub static JWK_KEY_TYPE: &'static str = "EC";
/// The 'crv' value of a K-256 key JWK
pub static JWK_CURVE: &'static str = "secp256k1";

type FieldSize = elliptic_curve::FieldSize<k256::Secp256k1>;

/// A K-256 (secp256k1) public key or keypair
#[derive(Clone, Debug)]
pub struct K256KeyPair {
    // SECURITY: SecretKey zeroizes on drop
    secret: Option<SecretKey>,
    public: PublicKey,
}

impl K256KeyPair {
    #[inline]
    pub(crate) fn from_secret_key(sk: SecretKey) -> Self {
        let pk = sk.public_key();
        Self {
            secret: Some(sk),
            public: pk,
        }
    }

    pub(crate) fn check_public_bytes(&self, pk: &[u8]) -> Result<(), Error> {
        if self.with_public_bytes(|slf| slf.ct_eq(pk)).into() {
            Ok(())
        } else {
            Err(err_msg!(InvalidKeyData, "invalid k256 keypair"))
        }
    }

    pub(crate) fn to_signing_key(&self) -> Option<SigningKey> {
        self.secret.as_ref().map(SigningKey::from)
    }

    /// Sign a message with the secret key
    pub fn sign(&self, message: &[u8]) -> Option<[u8; ES256K_SIGNATURE_LENGTH]> {
        if let Some(skey) = self.to_signing_key() {
            let sig: Signature = skey.sign(message);
            let sigb: [u8; 64] = sig.as_ref().try_into().unwrap();
            Some(sigb)
        } else {
            None
        }
    }

    /// Verify a signature with the public key
    pub fn verify_signature(&self, message: &[u8], signature: &[u8]) -> bool {
        if let Ok(sig) = Signature::try_from(signature) {
            let vk = VerifyingKey::from(self.public.as_affine());
            vk.verify(message, &sig).is_ok()
        } else {
            false
        }
    }
}

impl HasKeyAlg for K256KeyPair {
    fn algorithm(&self) -> KeyAlg {
        KeyAlg::EcCurve(EcCurves::Secp256k1)
    }
}

impl KeyMeta for K256KeyPair {
    type KeySize = U32;
}

impl KeyGen for K256KeyPair {
    fn generate(mut rng: impl KeyMaterial) -> Result<Self, Error> {
        ArrayKey::<FieldSize>::temp(|buf| loop {
            rng.read_okm(buf);
            if let Ok(key) = SecretKey::from_bytes(&buf) {
                return Ok(Self::from_secret_key(key));
            }
        })
    }
}

impl KeySecretBytes for K256KeyPair {
    fn from_secret_bytes(key: &[u8]) -> Result<Self, Error> {
        Ok(Self::from_secret_key(
            SecretKey::from_bytes(key).map_err(|_| err_msg!(InvalidKeyData))?,
        ))
    }

    fn with_secret_bytes<O>(&self, f: impl FnOnce(Option<&[u8]>) -> O) -> O {
        if let Some(sk) = self.secret.as_ref() {
            f(Some(sk.as_scalar_bytes().as_ref()))
        } else {
            f(None)
        }
    }
}

impl KeypairMeta for K256KeyPair {
    type PublicKeySize = U33;
    type KeypairSize = U65;
}

impl KeypairBytes for K256KeyPair {
    fn from_keypair_bytes(kp: &[u8]) -> Result<Self, Error> {
        if kp.len() != KEYPAIR_LENGTH {
            return Err(err_msg!(InvalidKeyData));
        }
        let result = K256KeyPair::from_secret_bytes(&kp[..SECRET_KEY_LENGTH])
            .map_err(|_| err_msg!(InvalidKeyData))?;
        result.check_public_bytes(&kp[SECRET_KEY_LENGTH..])?;
        Ok(result)
    }

    fn with_keypair_bytes<O>(&self, f: impl FnOnce(Option<&[u8]>) -> O) -> O {
        if let Some(secret) = self.secret.as_ref() {
            ArrayKey::<<Self as KeypairMeta>::KeypairSize>::temp(|arr| {
                let sk_b = secret.as_scalar_bytes();
                let pk_enc = EncodedPoint::encode(self.public, true);
                arr[..SECRET_KEY_LENGTH].copy_from_slice(sk_b.as_ref());
                arr[SECRET_KEY_LENGTH..].copy_from_slice(pk_enc.as_ref());
                f(Some(&*arr))
            })
        } else {
            f(None)
        }
    }
}

impl KeyPublicBytes for K256KeyPair {
    fn from_public_bytes(key: &[u8]) -> Result<Self, Error> {
        let pk = EncodedPoint::from_bytes(key)
            .and_then(|pt| pt.decode())
            .map_err(|_| err_msg!(InvalidKeyData))?;
        Ok(Self {
            secret: None,
            public: pk,
        })
    }

    fn with_public_bytes<O>(&self, f: impl FnOnce(&[u8]) -> O) -> O {
        let pt = EncodedPoint::encode(self.public, true);
        f(pt.as_ref())
    }
}

impl KeySign for K256KeyPair {
    fn write_signature(
        &self,
        message: &[u8],
        sig_type: Option<SignatureType>,
        out: &mut dyn WriteBuffer,
    ) -> Result<(), Error> {
        match sig_type {
            None | Some(SignatureType::ES256K) => {
                if let Some(sig) = self.sign(message) {
                    out.buffer_write(&sig[..])?;
                    Ok(())
                } else {
                    Err(err_msg!(Unsupported, "Undefined secret key"))
                }
            }
            #[allow(unreachable_patterns)]
            _ => Err(err_msg!(Unsupported, "Unsupported signature type")),
        }
    }
}

impl KeySigVerify for K256KeyPair {
    fn verify_signature(
        &self,
        message: &[u8],
        signature: &[u8],
        sig_type: Option<SignatureType>,
    ) -> Result<bool, Error> {
        match sig_type {
            None | Some(SignatureType::ES256K) => Ok(self.verify_signature(message, signature)),
            #[allow(unreachable_patterns)]
            _ => Err(err_msg!(Unsupported, "Unsupported signature type")),
        }
    }
}

impl ToJwk for K256KeyPair {
    fn encode_jwk(&self, enc: &mut dyn JwkEncoder) -> Result<(), Error> {
        let pk_enc = EncodedPoint::encode(self.public, false);
        let (x, y) = match pk_enc.coordinates() {
            Coordinates::Identity => {
                return Err(err_msg!(
                    Unsupported,
                    "Cannot convert identity point to JWK"
                ))
            }
            Coordinates::Uncompressed { x, y } => (x, y),
            Coordinates::Compressed { .. } | Coordinates::Compact { .. } => unreachable!(),
        };

        enc.add_str("crv", JWK_CURVE)?;
        enc.add_str("kty", JWK_KEY_TYPE)?;
        enc.add_as_base64("x", &x[..])?;
        enc.add_as_base64("y", &y[..])?;
        if enc.is_secret() {
            self.with_secret_bytes(|buf| {
                if let Some(sk) = buf {
                    enc.add_as_base64("d", sk)
                } else {
                    Ok(())
                }
            })?;
        }
        Ok(())
    }
}

impl FromJwk for K256KeyPair {
    fn from_jwk_parts(jwk: JwkParts<'_>) -> Result<Self, Error> {
        let pk_x = ArrayKey::<FieldSize>::try_new_with(|arr| {
            if jwk.x.decode_base64(arr)? != arr.len() {
                Err(err_msg!(InvalidKeyData))
            } else {
                Ok(())
            }
        })?;
        let pk_y = ArrayKey::<FieldSize>::try_new_with(|arr| {
            if jwk.y.decode_base64(arr)? != arr.len() {
                Err(err_msg!(InvalidKeyData))
            } else {
                Ok(())
            }
        })?;
        let pk = EncodedPoint::from_affine_coordinates(pk_x.as_ref(), pk_y.as_ref(), false)
            .decode()
            .map_err(|_| err_msg!(InvalidKeyData))?;
        if jwk.d.is_some() {
            ArrayKey::<FieldSize>::temp(|arr| {
                if jwk.d.decode_base64(arr)? != arr.len() {
                    Err(err_msg!(InvalidKeyData))
                } else {
                    let kp = K256KeyPair::from_secret_bytes(arr)?;
                    if kp.public != pk {
                        Err(err_msg!(InvalidKeyData))
                    } else {
                        Ok(kp)
                    }
                }
            })
        } else {
            Ok(Self {
                secret: None,
                public: pk,
            })
        }
    }
}

impl KeyExchange for K256KeyPair {
    fn write_key_exchange(&self, other: &Self, out: &mut dyn WriteBuffer) -> Result<(), Error> {
        match self.secret.as_ref() {
            Some(sk) => {
                let xk = diffie_hellman(sk.to_secret_scalar(), other.public.as_affine());
                out.buffer_write(xk.as_bytes())?;
                Ok(())
            }
            None => Err(err_msg!(MissingSecretKey)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repr::ToPublicBytes;

    #[test]
    fn jwk_expected() {
        // from https://identity.foundation/EcdsaSecp256k1RecoverySignature2020/
        // {"kty":"EC",
        // "crv":"secp256k1",
        // "d": "rhYFsBPF9q3-uZThy7B3c4LDF_8wnozFUAEm5LLC4Zw",
        // "kid": "JUvpllMEYUZ2joO59UNui_XYDqxVqiFLLAJ8klWuPBw",
        // "kty": "EC",
        // "x": "dWCvM4fTdeM0KmloF57zxtBPXTOythHPMm1HCLrdd3A",
        // "y": "36uMVGM7hnw-N6GnjFcihWE3SkrhMLzzLCdPMXPEXlA"
        // }
        let test_pvt_b64 = "rhYFsBPF9q3-uZThy7B3c4LDF_8wnozFUAEm5LLC4Zw";
        let test_pub_b64 = (
            "dWCvM4fTdeM0KmloF57zxtBPXTOythHPMm1HCLrdd3A",
            "36uMVGM7hnw-N6GnjFcihWE3SkrhMLzzLCdPMXPEXlA",
        );
        let test_pvt = base64::decode_config(test_pvt_b64, base64::URL_SAFE).unwrap();
        let sk = K256KeyPair::from_secret_bytes(&test_pvt).expect("Error creating signing key");

        let jwk = sk.to_jwk_public(None).expect("Error converting key to JWK");
        let jwk = JwkParts::from_str(&jwk).expect("Error parsing JWK");
        assert_eq!(jwk.kty, "EC");
        assert_eq!(jwk.crv, JWK_CURVE);
        assert_eq!(jwk.x, test_pub_b64.0);
        assert_eq!(jwk.y, test_pub_b64.1);
        assert_eq!(jwk.d, None);
        let pk_load = K256KeyPair::from_jwk_parts(jwk).unwrap();
        assert_eq!(sk.to_public_bytes(), pk_load.to_public_bytes());

        let jwk = sk.to_jwk_secret(None).expect("Error converting key to JWK");
        let jwk = JwkParts::from_slice(&jwk).expect("Error parsing JWK");
        assert_eq!(jwk.kty, "EC");
        assert_eq!(jwk.crv, JWK_CURVE);
        assert_eq!(jwk.x, test_pub_b64.0);
        assert_eq!(jwk.y, test_pub_b64.1);
        assert_eq!(jwk.d, test_pvt_b64);
        let sk_load = K256KeyPair::from_jwk_parts(jwk).unwrap();
        assert_eq!(
            sk.to_keypair_bytes().unwrap(),
            sk_load.to_keypair_bytes().unwrap()
        );
    }

    #[test]
    fn sign_verify_expected() {
        let test_msg = b"This is a dummy message for use with tests";
        let test_sig = &hex!(
            "a2a3affbe18cda8c5a7b6375f05b304c2303ab8beb21428709a43a519f8f946f
            6ffa7966afdb337e9b1f70bb575282e71d4fe5bbe6bfa97b229d6bd7e97df1e5"
        );
        let test_pvt = base64::decode_config(
            "jv_VrhPomm6_WOzb74xF4eMI0hu9p0W1Zlxi0nz8AFs",
            base64::URL_SAFE_NO_PAD,
        )
        .unwrap();
        let kp = K256KeyPair::from_secret_bytes(&test_pvt).unwrap();
        let sig = kp.sign(&test_msg[..]).unwrap();
        assert_eq!(sig, &test_sig[..]);
        assert_eq!(kp.verify_signature(&test_msg[..], &sig[..]), true);
        assert_eq!(kp.verify_signature(b"Not the message", &sig[..]), false);
        assert_eq!(kp.verify_signature(&test_msg[..], &[0u8; 64]), false);
    }

    #[test]
    fn key_exchange_random() {
        let kp1 = K256KeyPair::random().unwrap();
        let kp2 = K256KeyPair::random().unwrap();
        assert_ne!(
            kp1.to_keypair_bytes().unwrap(),
            kp2.to_keypair_bytes().unwrap()
        );

        let xch1 = kp1.key_exchange_bytes(&kp2).unwrap();
        let xch2 = kp2.key_exchange_bytes(&kp1).unwrap();
        assert_eq!(xch1.len(), 32);
        assert_eq!(xch1, xch2);
    }

    #[test]
    fn round_trip_bytes() {
        let kp = K256KeyPair::random().unwrap();
        let cmp = K256KeyPair::from_keypair_bytes(&kp.to_keypair_bytes().unwrap()).unwrap();
        assert_eq!(
            kp.to_keypair_bytes().unwrap(),
            cmp.to_keypair_bytes().unwrap()
        );
    }
}
