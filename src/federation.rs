//! Haruka 实例联邦协议：域名规范化、配对密钥派生、事件哈希链与加密交换。
//!
//! 安全模型：配对码是带外传递的一次性预共享秘密，握手与事件负载均用其
//! 派生的 XChaCha20-Poly1305 密钥加密，线路上只出现配对码哈希；配对密钥
//! 明文存于数据库以便实例锁定期间仍能接收消息，联邦业务数据均用配对密钥
//! 加密存储，与主 DEK 模型相互独立（数据库泄露可解联邦数据，文档已注明）。

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use sha2::{Digest, Sha256};

use crate::crypto;

/// 规范化实例域名：补全 scheme、去掉路径与尾斜杠，仅保留 scheme://host[:port]。
pub fn normalize_domain(input: &str) -> Result<String, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("域名不能为空".into());
    }
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    let (scheme, rest) = with_scheme
        .split_once("://")
        .ok_or_else(|| "域名格式不正确".to_string())?;
    if scheme != "https" && scheme != "http" {
        return Err("域名仅支持 http/https".into());
    }
    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .trim();
    if authority.is_empty()
        || !authority
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']'))
    {
        return Err("域名包含非法字符".into());
    }
    Ok(format!("{scheme}://{authority}"))
}

/// 由一次性配对码派生配对密钥（双方各自计算，结果一致）。
pub fn pairing_key(code: &[u8]) -> crypto::Dek {
    let mut material = b"haruka instance pairing v1\0".to_vec();
    material.extend_from_slice(code);
    crypto::Dek::new(Sha256::digest(&material).into())
}

pub fn pairing_code_hash(code: &[u8]) -> Vec<u8> {
    Sha256::digest(code).to_vec()
}

/// 事件链式摘要：前序摘要参与计算，双方对同一密文可各自重算验证。
pub fn event_digest(
    federation_id: &str,
    seq: i64,
    kind: &str,
    previous: &[u8],
    payload_cipher: &str,
) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(b"haruka federation event v1\0");
    for part in [
        federation_id.as_bytes(),
        &seq.to_be_bytes(),
        kind.as_bytes(),
        previous,
        payload_cipher.as_bytes(),
    ] {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part);
    }
    hasher.finalize().to_vec()
}

pub fn reverse_kind(kind: &str) -> &'static str {
    match kind {
        "borrow" => "lend",
        "lend" => "borrow",
        _ => "borrow",
    }
}

pub fn new_federation_id() -> String {
    let bytes = crypto::random_bytes::<16>();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn new_pairing_code() -> (String, Vec<u8>) {
    let bytes = crypto::random_bytes::<32>().to_vec();
    (URL_SAFE_NO_PAD.encode(&bytes), bytes)
}

pub fn decode_code(code: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(code.trim()).ok()
}

/// 联邦消息统一信封（JSON）。
#[derive(serde::Serialize, serde::Deserialize)]
pub struct PairRequest {
    pub code_hash: String,
    pub cipher: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct EventMessage {
    pub from_domain: String,
    pub federation_id: String,
    pub seq: i64,
    pub kind: String,
    pub cipher: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct StatusMessage {
    pub from_domain: String,
    pub federation_id: String,
    pub cipher: String,
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(12))
        .user_agent("haruka-federation/1")
        .build()
        .map_err(|error| format!("联邦 HTTP 客户端初始化失败：{error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domain_normalization_and_key_derivation_are_deterministic() {
        assert_eq!(
            normalize_domain("haruka.example.com/path/").unwrap(),
            "https://haruka.example.com"
        );
        assert_eq!(
            normalize_domain("http://localhost:3111/").unwrap(),
            "http://localhost:3111"
        );
        assert!(normalize_domain("ftp://x").is_err());
        let code = b"pairing-code";
        assert_eq!(pairing_key(code), pairing_key(code));
        let first = event_digest("fid", 1, "created", &[], "cipher");
        let second = event_digest("fid", 2, "confirmed", &first, "cipher2");
        assert_ne!(first, second);
        assert_eq!(reverse_kind("borrow"), "lend");
        assert_eq!(reverse_kind("lend"), "borrow");
    }
}
