//! Telegram three-message DH exchange. Exponentiation uses fixed-width,
//! constant-time Montgomery arithmetic; only the published safe prime is accepted.
use crate::{Error, Result};
use crypto_bigint::{
    Encoding, U2048,
    modular::{MontyForm, MontyParams},
};
use sha1::Sha1;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

pub const TELEGRAM_PRIME_HEX: &str = concat!(
    "C71CAEB9C6B1C9048E6C522F70F13F73980D40238E3E21C14934D037563D930F",
    "48198A0AA7C14058229493D22530F4DBFA336F6E0AC925139543AED44CCE7C37",
    "20FD51F69458705AC68CD4FE6B6B13ABDC9746512969328454F18FAF8C595F64",
    "2477FE96BB2A941D5BCD1D4AC8CC49880708FA9B378E3C4F3A9060BEE67CF9A",
    "4A4A695811051907E162753B56B0F6B410DBA74D8A84B2A14B3144E0EF1284754",
    "FD17ED950D5965B4B9DD46582DB1178D169C6BC465B0D6FF9CA3928FEF5B9AE4",
    "E418FC15E83EBEA0F87FA9FF5EED70050DED2849F47BF959D956850CE929851F0",
    "D8115F635B105EE2E4E15D04B2454BF6F4FADF034B10403119CD8E3B92FCC5B"
);

pub struct AuthKey(pub(crate) Zeroizing<[u8; 256]>);
impl AuthKey {
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let key: [u8; 256] = bytes
            .try_into()
            .map_err(|_| Error::Invalid("auth key must contain 256 bytes".into()))?;
        if key.iter().all(|b| *b == 0) {
            return Err(Error::Invalid("zero auth key".into()));
        }
        Ok(Self(Zeroizing::new(key)))
    }
    pub fn fingerprint(&self) -> i64 {
        let digest = Sha1::digest(self.0.as_slice());
        let mut bytes = [0; 8];
        bytes.copy_from_slice(&digest[12..20]);
        i64::from_le_bytes(bytes)
    }
}

pub struct KeyExchange {
    exponent: Zeroizing<U2048>,
    public: [u8; 256],
    commitment: Option<[u8; 32]>,
    consumed: bool,
}

fn params() -> Result<MontyParams<{ U2048::LIMBS }>> {
    let prime = U2048::from_be_hex(TELEGRAM_PRIME_HEX);
    let odd = Option::from(prime.to_odd())
        .ok_or_else(|| Error::Invalid("invalid pinned DH prime".into()))?;
    Ok(MontyParams::new_vartime(odd)) // modulus is public, never the secret exponent
}

fn check_public(value: &U2048) -> Result<()> {
    let prime = U2048::from_be_hex(TELEGRAM_PRIME_HEX);
    let limit = U2048::ONE.shl_vartime(2048 - 64);
    if value <= &limit || value >= &prime.wrapping_sub(&limit) {
        return Err(Error::Invalid("unsafe DH public value".into()));
    }
    Ok(())
}

impl KeyExchange {
    pub fn new(
        g: u32,
        prime: &[u8],
        server_random: &[u8],
        commitment: Option<&[u8]>,
    ) -> Result<Self> {
        if prime != U2048::from_be_hex(TELEGRAM_PRIME_HEX).to_be_bytes()
            || g != 3
            || server_random.len() != 256
        {
            return Err(Error::Invalid(
                "only Telegram's pinned 2048-bit safe prime and generator 3 are accepted".into(),
            ));
        }
        let commitment = commitment
            .map(|value| {
                value
                    .try_into()
                    .map_err(|_| Error::Invalid("g_a_hash must contain 32 bytes".into()))
            })
            .transpose()?;
        let params = params()?;
        for _ in 0..32 {
            let mut bytes = Zeroizing::new([0u8; 256]);
            getrandom::fill(bytes.as_mut()).map_err(|e| Error::Invalid(format!("OS RNG: {e}")))?;
            for (a, b) in bytes.iter_mut().zip(server_random) {
                *a ^= *b;
            }
            let exponent = Zeroizing::new(U2048::from_be_bytes(*bytes));
            let public = MontyForm::new(&U2048::from_u32(g), params)
                .pow(&exponent)
                .retrieve();
            if check_public(&public).is_ok() {
                return Ok(Self {
                    exponent,
                    public: public.to_be_bytes(),
                    commitment,
                    consumed: false,
                });
            }
        }
        Err(Error::Invalid(
            "DH public generation budget exceeded".into(),
        ))
    }

    pub fn first_message(&self) -> Vec<u8> {
        if self.commitment.is_some() {
            self.public.to_vec()
        } else {
            Sha256::digest(self.public).to_vec()
        }
    }
    pub fn public(&self) -> &[u8; 256] {
        &self.public
    }

    pub fn complete(
        &mut self,
        remote: &[u8],
        expected_fingerprint: Option<i64>,
    ) -> Result<(AuthKey, [u8; 32])> {
        if self.consumed {
            return Err(Error::Invalid("DH exchange already consumed".into()));
        }
        self.consumed = true; // reject duplicates and alternative versions of the exchange
        let remote: [u8; 256] = remote
            .try_into()
            .map_err(|_| Error::Invalid("DH public value must contain 256 bytes".into()))?;
        if let Some(commitment) = self.commitment {
            if !bool::from(Sha256::digest(remote).as_slice().ct_eq(&commitment)) {
                return Err(Error::Invalid("g_a hash commitment mismatch".into()));
            }
            if expected_fingerprint.is_none() {
                return Err(Error::Invalid(
                    "incoming exchange requires key fingerprint".into(),
                ));
            }
        }
        let value = U2048::from_be_bytes(remote);
        check_public(&value)?;
        let key = AuthKey(Zeroizing::new(
            MontyForm::new(&value, params()?)
                .pow(&self.exponent)
                .retrieve()
                .to_be_bytes(),
        ));
        if let Some(expected) = expected_fingerprint
            && !bool::from(
                key.fingerprint()
                    .to_le_bytes()
                    .ct_eq(&expected.to_le_bytes()),
            )
        {
            return Err(Error::Invalid("DH key fingerprint mismatch".into()));
        }
        let ga = if self.commitment.is_some() {
            remote
        } else {
            self.public
        };
        let mut digest = Sha256::new();
        digest.update(key.0.as_slice());
        digest.update(ga);
        Ok((key, digest.finalize().into()))
    }
}
