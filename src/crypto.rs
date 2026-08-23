use argon2::Argon2;
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use bip39::{Language, Mnemonic};
use chacha20poly1305::{
    aead::{Aead, KeyInit},
    XChaCha20Poly1305, XNonce,
};
use rand::RngCore;
use zeroize::Zeroizing;

pub const DEK_LEN: usize = 32;
pub const SALT_LEN: usize = 16;
pub const NONCE_LEN: usize = 24;
pub const MNEMONIC_ENTROPY_LEN: usize = 16;

pub type Dek = Zeroizing<[u8; DEK_LEN]>;

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    bytes
}

/// 用 Argon2id 从密码派生 KEK
pub fn derive_kek(password: &str, salt: &[u8]) -> Zeroizing<[u8; DEK_LEN]> {
    let mut kek = Zeroizing::new([0u8; DEK_LEN]);
    Argon2::default()
        .hash_password_into(password.as_bytes(), salt, &mut *kek)
        .expect("argon2 派生失败");
    kek
}

/// 生成由 128 位系统随机熵编码而成的 BIP-39 英文 12 词助记词。
pub fn generate_recovery_mnemonic() -> Mnemonic {
    let entropy = Zeroizing::new(random_bytes::<MNEMONIC_ENTROPY_LEN>());
    Mnemonic::from_entropy_in(Language::English, entropy.as_slice())
        .expect("生成 BIP-39 助记词失败")
}

/// 解析并校验 BIP-39 英文 12 词助记词，容忍多余空白和大小写。
pub fn parse_recovery_mnemonic(phrase: &str) -> Option<Mnemonic> {
    let normalized = phrase
        .split_whitespace()
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>()
        .join(" ");
    let mnemonic = Mnemonic::parse_in_normalized(Language::English, &normalized).ok()?;
    (mnemonic.word_count() == 12).then_some(mnemonic)
}

/// 按 BIP-39 的种子派生规则生成专用于包裹 DEK 的恢复密钥。
pub fn derive_recovery_kek(mnemonic: &Mnemonic) -> Zeroizing<[u8; DEK_LEN]> {
    let seed = Zeroizing::new(mnemonic.to_seed_normalized("haruka recovery key v1"));
    let mut kek = Zeroizing::new([0u8; DEK_LEN]);
    kek.copy_from_slice(&seed[..DEK_LEN]);
    kek
}

fn cipher(key: &[u8]) -> XChaCha20Poly1305 {
    XChaCha20Poly1305::new_from_slice(key).expect("密钥长度错误")
}

/// 加密字节串，输出 base64(nonce || ciphertext)
pub fn encrypt(key: impl AsRef<[u8]>, plaintext: &[u8]) -> String {
    let nonce = random_bytes::<NONCE_LEN>();
    let ct = cipher(key.as_ref())
        .encrypt(XNonce::from_slice(&nonce), plaintext)
        .expect("加密失败");
    let mut blob = nonce.to_vec();
    blob.extend_from_slice(&ct);
    B64.encode(blob)
}

pub fn decrypt(key: impl AsRef<[u8]>, encoded: &str) -> Option<Vec<u8>> {
    let blob = B64.decode(encoded).ok()?;
    if blob.len() < NONCE_LEN {
        return None;
    }
    let (nonce, ct) = blob.split_at(NONCE_LEN);
    cipher(key.as_ref())
        .decrypt(XNonce::from_slice(nonce), ct)
        .ok()
}

/// 用 KEK 包裹 DEK，返回 (nonce, ciphertext)
pub fn wrap_dek(dek: &Dek, kek: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let nonce = random_bytes::<NONCE_LEN>();
    let ct = cipher(kek)
        .encrypt(XNonce::from_slice(&nonce), dek.as_slice())
        .expect("包裹 DEK 失败");
    (nonce.to_vec(), ct)
}

/// 用 KEK 解出 DEK；密码错误时返回 None
pub fn unwrap_dek(nonce: &[u8], ct: &[u8], kek: impl AsRef<[u8]>) -> Option<Dek> {
    let raw = cipher(kek.as_ref())
        .decrypt(XNonce::from_slice(nonce), ct)
        .ok()?;
    let arr: [u8; DEK_LEN] = raw.try_into().ok()?;
    Some(Zeroizing::new(arr))
}

pub fn decrypt_string(dek: &Dek, encoded: &str) -> String {
    decrypt(dek, encoded)
        .and_then(|v| String::from_utf8(v).ok())
        .unwrap_or_default()
}

/// 金额（整数分）的加解密
pub fn encrypt_cents(dek: &Dek, cents: i64) -> String {
    encrypt(dek, cents.to_string().as_bytes())
}

pub fn decrypt_cents(dek: &Dek, encoded: &str) -> i64 {
    decrypt_string(dek, encoded).parse().unwrap_or(0)
}
