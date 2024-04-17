use std::fmt;

use anyhow::Context;
use rand::Rng;

#[derive(Clone, Copy)]
pub struct Secret {
    key: [u8; 32],
}

impl TryFrom<&str> for Secret {
    type Error = anyhow::Error;

    fn try_from(s: &str) -> anyhow::Result<Self> {
        let key: [u8; 32] = hex::decode(s)
            .context("Couldn't hexdecode key")?
            .try_into()
            .map_err(|e: Vec<u8>| {
                anyhow::anyhow!("Hex-decoded key was {} bytes, not 32", e.len())
            })?;
        Ok(Self { key })
    }
}

impl TryFrom<Vec<u8>> for Secret {
    type Error = anyhow::Error;

    fn try_from(v: Vec<u8>) -> anyhow::Result<Self> {
        let key: [u8; 32] = v
            .try_into()
            .map_err(|e: Vec<u8>| anyhow::anyhow!("Key was {} bytes, not 32", e.len()))?;
        Ok(Self { key })
    }
}

impl Secret {
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.key
    }

    pub fn random() -> Self {
        Self {
            key: rand::thread_rng().gen(),
        }
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", hex::encode(self.key))
    }
}

pub type InstanceSecret = Secret;
