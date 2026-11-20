//! Web Push to an angel's phone (design.md, "Angels with their own key"): the payload is
//! encrypted to the subscription (RFC 8291, aes128gcm) and the request signed with the angel's
//! own VAPID key (RFC 8292). Outbound HTTPS only, straight to Apple's or Google's push service.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes128Gcm, Nonce};
use anyhow::{Context as _, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine as _;
use hkdf::Hkdf;
use p256::ecdsa::signature::Signer as _;
use p256::ecdsa::{Signature, SigningKey};
use p256::elliptic_curve::sec1::ToEncodedPoint as _;
use p256::{PublicKey, SecretKey};
use sha2::Sha256;

use warn_proto::PushSub;

/// How a push went.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Sent,
    /// The subscription no longer exists (the app was removed, or notifications switched off).
    Gone,
    Failed(String),
}

/// Encrypt a payload for a subscription (`ua_public`: its p256dh key, `auth`: its auth
/// secret), with this sender's one-time key and salt.
fn encrypt(payload: &[u8], ua_public: &[u8], auth: &[u8], as_secret: &SecretKey, salt: &[u8; 16]) -> Result<Vec<u8>> {
    let ua = PublicKey::from_sec1_bytes(ua_public).context("bad p256dh key")?;
    let shared = p256::ecdh::diffie_hellman(as_secret.to_nonzero_scalar(), ua.as_affine());
    let as_public = as_secret.public_key().to_encoded_point(false);
    let mut info = b"WebPush: info\0".to_vec();
    info.extend_from_slice(ua_public);
    info.extend_from_slice(as_public.as_bytes());
    let mut ikm = [0u8; 32];
    Hkdf::<Sha256>::new(Some(auth), shared.raw_secret_bytes())
        .expand(&info, &mut ikm)
        .map_err(|_| anyhow::anyhow!("hkdf"))?;
    let hk = Hkdf::<Sha256>::new(Some(salt), &ikm);
    let mut cek = [0u8; 16];
    let mut nonce = [0u8; 12];
    hk.expand(b"Content-Encoding: aes128gcm\0", &mut cek).map_err(|_| anyhow::anyhow!("hkdf"))?;
    hk.expand(b"Content-Encoding: nonce\0", &mut nonce).map_err(|_| anyhow::anyhow!("hkdf"))?;
    let mut plain = payload.to_vec();
    plain.push(2); // last (and only) record
    let cipher = Aes128Gcm::new_from_slice(&cek)
        .map_err(|_| anyhow::anyhow!("aes key"))?
        .encrypt(Nonce::from_slice(&nonce), plain.as_ref())
        .map_err(|_| anyhow::anyhow!("aes-gcm"))?;
    let mut body = salt.to_vec();
    body.extend_from_slice(&4096u32.to_be_bytes());
    body.push(as_public.as_bytes().len() as u8);
    body.extend_from_slice(as_public.as_bytes());
    body.extend_from_slice(&cipher);
    Ok(body)
}

/// The VAPID authorization header for this endpoint.
fn vapid_header(endpoint: &str, vapid_private: &str, vapid_public: &str, sub: &str, now: u64) -> Result<String> {
    let url = reqwest::Url::parse(endpoint).context("bad endpoint")?;
    let aud = url.origin().ascii_serialization();
    let header = B64.encode(br#"{"typ":"JWT","alg":"ES256"}"#);
    let claims = B64.encode(serde_json::to_vec(&serde_json::json!({ "aud": aud, "exp": now + 12 * 3600, "sub": sub }))?);
    let key = SigningKey::from_slice(&B64.decode(vapid_private).context("bad vapid key")?).context("bad vapid key")?;
    let signed = format!("{header}.{claims}");
    let sig: Signature = key.sign(signed.as_bytes());
    Ok(format!("vapid t={signed}.{}, k={vapid_public}", B64.encode(sig.to_bytes())))
}

/// Push a payload (JSON for the app's service worker) to one subscription.
pub async fn send(http: &reqwest::Client, push: &PushSub, payload: &[u8], sub: &str, now: u64) -> Outcome {
    match try_send(http, push, payload, sub, now).await {
        Ok(o) => o,
        Err(e) => Outcome::Failed(format!("{e:#}")),
    }
}

async fn try_send(http: &reqwest::Client, push: &PushSub, payload: &[u8], sub: &str, now: u64) -> Result<Outcome> {
    let as_secret = SecretKey::random(&mut rand_core::OsRng);
    let mut salt = [0u8; 16];
    rand_core::RngCore::fill_bytes(&mut rand_core::OsRng, &mut salt);
    let body = encrypt(payload, &B64.decode(&push.p256dh)?, &B64.decode(&push.auth)?, &as_secret, &salt)?;
    let auth = vapid_header(&push.endpoint, &push.vapid_private, &push.vapid_public, sub, now)?;
    let resp = http
        .post(&push.endpoint)
        .header("TTL", "86400")
        .header("Urgency", "high")
        .header("Content-Encoding", "aes128gcm")
        .header("Content-Type", "application/octet-stream")
        .header("Authorization", auth)
        .body(body)
        .send()
        .await?;
    let status = resp.status().as_u16();
    Ok(match status {
        200..=299 => Outcome::Sent,
        404 | 410 => Outcome::Gone,
        _ => Outcome::Failed(format!("{status} {}", resp.text().await.unwrap_or_default().chars().take(200).collect::<String>())),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::ecdsa::signature::Verifier as _;
    use p256::ecdsa::VerifyingKey;

    /// RFC 8291, Appendix A.
    #[test]
    fn matches_the_rfc_example() {
        let as_secret = SecretKey::from_slice(&B64.decode("yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw").unwrap()).unwrap();
        let ua_public = B64
            .decode("BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4")
            .unwrap();
        let auth = B64.decode("BTBZMqHH6r4Tts7J_aSIgg").unwrap();
        let salt: [u8; 16] = B64.decode("DGv6ra1nlYgDCS1FRnbzlw").unwrap().try_into().unwrap();
        let body = encrypt(b"When I grow up, I want to be a watermelon", &ua_public, &auth, &as_secret, &salt).unwrap();
        assert_eq!(
            B64.encode(body),
            "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPTpK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN"
        );
    }

    #[test]
    fn vapid_token_is_signed_by_the_angels_key() {
        let sk = SecretKey::random(&mut rand_core::OsRng);
        let public = B64.encode(sk.public_key().to_encoded_point(false).as_bytes());
        let h = vapid_header("https://web.push.apple.com/abc/def", &B64.encode(sk.to_bytes()), &public, "mailto:a@b.invalid", 1000)
            .unwrap();
        let (t, k) = h.strip_prefix("vapid t=").unwrap().split_once(", k=").unwrap();
        assert_eq!(k, public);
        let parts: Vec<&str> = t.split('.').collect();
        let claims: serde_json::Value = serde_json::from_slice(&B64.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(claims["aud"], "https://web.push.apple.com");
        assert_eq!(claims["exp"], 1000 + 12 * 3600);
        let sig = Signature::from_slice(&B64.decode(parts[2]).unwrap()).unwrap();
        let vk = VerifyingKey::from(sk.public_key());
        vk.verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &sig).unwrap();
    }
}
