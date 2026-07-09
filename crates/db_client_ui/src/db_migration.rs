use std::collections::BTreeMap;

use aes::Aes256;
use anyhow::{Context as _, Result, anyhow};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use cbc::cipher::block_padding::Pkcs7;
use cbc::cipher::{BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use db_client::{ConnectionConfig, ConnectionId, Folder};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use zeroize::Zeroize;

type Aes256CbcEnc = cbc::Encryptor<Aes256>;
type Aes256CbcDec = cbc::Decryptor<Aes256>;
type HmacSha256 = Hmac<Sha256>;

/// PBKDF2 work factor. Higher is slower to brute-force; this is a one-time cost
/// per export/import so a large value is acceptable.
const PBKDF2_ITERATIONS: u32 = 600_000;
const SALT_LEN: usize = 16;
const IV_LEN: usize = 16;
/// 32 bytes for the AES key + 32 bytes for the HMAC key, derived together.
const KEY_MATERIAL_LEN: usize = 64;

/// Current bundle format version. Bumped if the on-disk shape changes.
pub const BUNDLE_VERSION: u32 = 1;

/// A SQL console file captured verbatim so the destination machine restores the
/// exact query text the user had per connection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConsoleFile {
    pub connection_id: ConnectionId,
    pub filename: String,
    pub content: String,
}

/// Connection passwords encrypted with a key derived from the user's master
/// password (PBKDF2-HMAC-SHA256), AES-256-CBC, authenticated with HMAC-SHA256
/// over salt + iterations + IV + ciphertext (encrypt-then-MAC). The salt, IV and
/// iteration count are not secret and travel in the clear.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EncryptedSecrets {
    pub kdf_salt: String,
    pub iterations: u32,
    pub iv: String,
    pub ciphertext: String,
    pub mac: String,
}

/// Both secrets a connection may hold: its own DB password and, independently,
/// its SSH tunnel password. Empty string means "not set", matching how
/// `ConnectionConfig` itself represents an absent password.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ConnectionSecrets {
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub ssh_password: String,
}

/// The whole Database Explorer state in one portable file: the folder tree, all
/// connection settings (passwords redacted here — they live in `secrets`), the
/// SQL console files, and optionally the encrypted passwords.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportBundle {
    pub version: u32,
    #[serde(default)]
    pub folders: Vec<Folder>,
    #[serde(default)]
    pub connections: Vec<ConnectionConfig>,
    #[serde(default)]
    pub consoles: Vec<ConsoleFile>,
    #[serde(default)]
    pub secrets: Option<EncryptedSecrets>,
}

fn derive_key_material(
    master_password: &str,
    salt: &[u8],
    iterations: u32,
) -> [u8; KEY_MATERIAL_LEN] {
    let mut material = [0u8; KEY_MATERIAL_LEN];
    pbkdf2::pbkdf2::<HmacSha256>(master_password.as_bytes(), salt, iterations, &mut material);
    material
}

fn mac_tag(
    mac_key: &[u8],
    salt: &[u8],
    iterations: u32,
    iv: &[u8],
    ciphertext: &[u8],
) -> Result<Vec<u8>> {
    let mut mac =
        HmacSha256::new_from_slice(mac_key).map_err(|_| anyhow!("invalid MAC key length"))?;
    mac.update(salt);
    mac.update(&iterations.to_le_bytes());
    mac.update(iv);
    mac.update(ciphertext);
    Ok(mac.finalize().into_bytes().to_vec())
}

/// Encrypts the per-connection secrets map under `master_password`. The map is
/// serialized to JSON, padded, AES-256-CBC encrypted, then MAC'd.
pub fn encrypt_secrets(
    connection_secrets: &BTreeMap<ConnectionId, ConnectionSecrets>,
    master_password: &str,
) -> Result<EncryptedSecrets> {
    let mut salt = [0u8; SALT_LEN];
    getrandom::getrandom(&mut salt).map_err(|err| anyhow!("failed to generate salt: {err}"))?;
    let mut iv = [0u8; IV_LEN];
    getrandom::getrandom(&mut iv).map_err(|err| anyhow!("failed to generate IV: {err}"))?;

    let mut material = derive_key_material(master_password, &salt, PBKDF2_ITERATIONS);
    let (enc_key, mac_key) = material.split_at(32);

    let mut plaintext = serde_json::to_vec(connection_secrets).context("serializing secrets")?;
    let ciphertext = Aes256CbcEnc::new_from_slices(enc_key, &iv)
        .map_err(|_| anyhow!("invalid key/iv length"))?
        .encrypt_padded_vec_mut::<Pkcs7>(&plaintext);
    let tag = mac_tag(mac_key, &salt, PBKDF2_ITERATIONS, &iv, &ciphertext)?;

    plaintext.zeroize();
    material.zeroize();

    Ok(EncryptedSecrets {
        kdf_salt: BASE64.encode(salt),
        iterations: PBKDF2_ITERATIONS,
        iv: BASE64.encode(iv),
        ciphertext: BASE64.encode(&ciphertext),
        mac: BASE64.encode(&tag),
    })
}

/// Decrypts the per-connection secrets map. Returns an error (never garbage)
/// when the master password is wrong or the data was tampered with — the HMAC
/// check fails first.
pub fn decrypt_secrets(
    secrets: &EncryptedSecrets,
    master_password: &str,
) -> Result<BTreeMap<ConnectionId, ConnectionSecrets>> {
    let salt = BASE64.decode(&secrets.kdf_salt).context("decoding salt")?;
    let iv = BASE64.decode(&secrets.iv).context("decoding IV")?;
    let ciphertext = BASE64
        .decode(&secrets.ciphertext)
        .context("decoding ciphertext")?;
    let expected_mac = BASE64.decode(&secrets.mac).context("decoding MAC")?;

    let mut material = derive_key_material(master_password, &salt, secrets.iterations);
    let (enc_key, mac_key) = material.split_at(32);

    let mut mac =
        HmacSha256::new_from_slice(mac_key).map_err(|_| anyhow!("invalid MAC key length"))?;
    mac.update(&salt);
    mac.update(&secrets.iterations.to_le_bytes());
    mac.update(&iv);
    mac.update(&ciphertext);
    let mac_ok = mac.verify_slice(&expected_mac).is_ok();
    if !mac_ok {
        material.zeroize();
        return Err(anyhow!("invalid master password or corrupted export"));
    }

    let plaintext = Aes256CbcDec::new_from_slices(enc_key, &iv)
        .map_err(|_| anyhow!("invalid key/iv length"))?
        .decrypt_padded_vec_mut::<Pkcs7>(&ciphertext)
        .map_err(|_| anyhow!("invalid master password or corrupted export"))?;
    material.zeroize();

    let secrets = serde_json::from_slice(&plaintext).context("deserializing decrypted secrets")?;
    Ok(secrets)
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn sample_map() -> BTreeMap<ConnectionId, ConnectionSecrets> {
        let mut map = BTreeMap::new();
        map.insert(
            Uuid::new_v4(),
            ConnectionSecrets {
                password: "p@ss w0rd:1".to_string(),
                ssh_password: String::new(),
            },
        );
        map.insert(
            Uuid::new_v4(),
            ConnectionSecrets {
                password: "second-secret".to_string(),
                ssh_password: "tunnel-secret".to_string(),
            },
        );
        map
    }

    #[test]
    fn secrets_round_trip() {
        let map = sample_map();
        let encrypted = encrypt_secrets(&map, "master").unwrap();
        let decrypted = decrypt_secrets(&encrypted, "master").unwrap();
        assert_eq!(map, decrypted);
    }

    #[test]
    fn secrets_round_trip_preserves_the_ssh_password_independently_of_the_db_password() {
        let map = sample_map();
        let encrypted = encrypt_secrets(&map, "master").unwrap();
        let decrypted = decrypt_secrets(&encrypted, "master").unwrap();
        for (id, secrets) in &map {
            let restored = &decrypted[id];
            assert_eq!(restored.password, secrets.password);
            assert_eq!(restored.ssh_password, secrets.ssh_password);
        }
    }

    #[test]
    fn wrong_master_password_errors() {
        let map = sample_map();
        let encrypted = encrypt_secrets(&map, "correct").unwrap();
        assert!(decrypt_secrets(&encrypted, "wrong").is_err());
    }

    #[test]
    fn empty_map_round_trips() {
        let map = BTreeMap::new();
        let encrypted = encrypt_secrets(&map, "master").unwrap();
        let decrypted = decrypt_secrets(&encrypted, "master").unwrap();
        assert!(decrypted.is_empty());
    }

    #[test]
    fn tampered_ciphertext_errors() {
        let map = sample_map();
        let mut encrypted = encrypt_secrets(&map, "master").unwrap();
        let mut raw = BASE64.decode(&encrypted.ciphertext).unwrap();
        raw[0] ^= 0xff;
        encrypted.ciphertext = BASE64.encode(&raw);
        assert!(decrypt_secrets(&encrypted, "master").is_err());
    }

    #[test]
    fn bundle_serde_round_trips() {
        let bundle = ExportBundle {
            version: BUNDLE_VERSION,
            folders: Vec::new(),
            connections: Vec::new(),
            consoles: vec![ConsoleFile {
                connection_id: Uuid::new_v4(),
                filename: "db-1234.sql".to_string(),
                content: "SELECT 1;".to_string(),
            }],
            secrets: None,
        };
        let json = serde_json::to_vec(&bundle).unwrap();
        let parsed: ExportBundle = serde_json::from_slice(&json).unwrap();
        assert_eq!(parsed.consoles, bundle.consoles);
        assert_eq!(parsed.version, BUNDLE_VERSION);
    }
}
