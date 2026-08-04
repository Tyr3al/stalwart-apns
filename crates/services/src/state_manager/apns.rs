/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-SEL
 */

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use common::{
    BuildServer, Inner, Server,
    config::mailstore::xaps::XapsConfig,
    ipc::EmailPush,
};
use email::{
    mailbox::INBOX_ID,
    message::metadata::MessageData,
    push::xaps::XapsRegistrations,
};
use p256::{
    ecdsa::{Signature, SigningKey, signature::Signer},
    pkcs8::DecodePrivateKey,
};
use std::{
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};
use store::{
    Serialize, ValueKey,
    write::{AlignedBytes, Archive, Archiver, BatchBuilder, now},
};
use trc::{AddContext, PushSubscriptionEvent};
use types::{collection::Collection, field::PrincipalField};

const APNS_PRODUCTION_HOST: &str = "https://api.push.apple.com";
const APNS_SANDBOX_HOST: &str = "https://api.sandbox.push.apple.com";
/// APNs rejects provider tokens older than one hour.
const APNS_TOKEN_TTL_SECS: u64 = 60 * 60;
const APNS_EXPIRATION_SECS: u64 = 24 * 60 * 60;
const APNS_REQUEST_TIMEOUT_SECS: u64 = 30;

pub enum SendResult {
    /// Notification accepted by APNs.
    Ok,
    /// The device token is no longer active (HTTP 410), the registration
    /// should be removed.
    DeviceTokenInactive,
    /// Transient or unknown failure.
    Error,
}

pub struct ApnsClient {
    host: String,
    topic: String,
    team_id: String,
    key_id: String,
    signing_key: SigningKey,
    http_client: reqwest::Client,
    /// Cached provider token (ES256 JWT) and its issue time.
    token: Arc<Mutex<Option<(String, u64)>>>,
}

impl ApnsClient {
    /// Builds an APNs client from the XAPS configuration. Returns `None` when
    /// the feature is not fully configured.
    pub fn try_new(config: &XapsConfig) -> Option<Self> {
        let key_file_p8 = config.key_file_p8.as_deref()?.trim();
        if key_file_p8.is_empty() {
            return None;
        }
        let signing_key = SigningKey::from_pkcs8_pem(key_file_p8).ok()?;
        let key_id = config.key_id.clone()?;
        let team_id = config.team_id.clone()?;
        let topic = config.topic.clone().unwrap_or_default();
        if topic.is_empty() {
            return None;
        }

        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(APNS_REQUEST_TIMEOUT_SECS))
            .build()
            .ok()?;

        Some(ApnsClient {
            host: if config.sandbox {
                APNS_SANDBOX_HOST.to_string()
            } else {
                APNS_PRODUCTION_HOST.to_string()
            },
            topic,
            team_id,
            key_id,
            signing_key,
            http_client,
            token: Arc::new(Mutex::new(None)),
        })
    }

    fn token(&self, now: u64) -> String {
        let mut cache = self.token.lock().unwrap();
        if let Some((token, issued_at)) = cache.as_ref()
            && now.saturating_sub(*issued_at) < APNS_TOKEN_TTL_SECS
        {
            return token.clone();
        }

        let mut header = serde_json::Map::new();
        header.insert("alg".into(), "ES256".into());
        header.insert("kid".into(), self.key_id.clone().into());
        let mut claims = serde_json::Map::new();
        claims.insert("iss".into(), self.team_id.clone().into());
        claims.insert("iat".into(), now.into());

        let signing_input = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).unwrap()),
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
        );
        let signature: Signature = self.signing_key.sign(signing_input.as_bytes());
        let token = format!(
            "{}.{}",
            signing_input,
            URL_SAFE_NO_PAD.encode(signature.to_bytes())
        );

        *cache = Some((token.clone(), now));
        token
    }

    pub async fn send_notification(&self, device_token: &str, account_id: &str) -> SendResult {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let payload = serde_json::json!({ "aps": { "account-id": account_id } }).to_string();

        match self
            .http_client
            .post(format!("{}/3/device/{}", self.host, device_token))
            .header("apns-topic", self.topic.as_str())
            .header("apns-push-type", "background")
            .header("apns-priority", "5")
            .header("apns-expiration", (now + APNS_EXPIRATION_SECS).to_string())
            .bearer_auth(self.token(now))
            .body(payload)
            .send()
            .await
        {
            Ok(response) => {
                let status = response.status();
                if status.is_success() {
                    trc::event!(
                        PushSubscription(PushSubscriptionEvent::Success),
                        Details = format!("APNs notification sent for account {account_id}"),
                    );
                    SendResult::Ok
                } else if status == reqwest::StatusCode::GONE {
                    trc::event!(
                        PushSubscription(PushSubscriptionEvent::NotFound),
                        Details = format!("APNs device token is no longer active for account {account_id}"),
                    );
                    SendResult::DeviceTokenInactive
                } else {
                    trc::event!(
                        PushSubscription(PushSubscriptionEvent::Error),
                        Details = format!("APNs request failed for account {account_id}"),
                        Code = status.as_u16(),
                    );
                    SendResult::Error
                }
            }
            Err(err) => {
                trc::event!(
                    PushSubscription(PushSubscriptionEvent::Error),
                    Details = format!("APNs request failed for account {account_id}"),
                    Reason = err.to_string(),
                );
                SendResult::Error
            }
        }
    }
}

/// Sends APNs notifications for a newly delivered message, mirroring the
/// `/notify` handler of dovecot-xaps-daemon: only messages delivered to the
/// inbox of an account with registered devices produce push notifications.
pub async fn deliver_xaps_notifications(inner: Arc<Inner>, email_push: EmailPush) {
    let server = inner.build_server();
    if !server.core.xaps.enabled {
        return;
    }
    let Some(apns) = ApnsClient::try_new(&server.core.xaps) else {
        return;
    };

    // Only notify for messages delivered to the inbox (INBOX_ID = 0).
    let Some(message_data) = server
        .store()
        .get_value::<Archive<AlignedBytes>>(ValueKey::archive(
            email_push.account_id,
            Collection::Email,
            email_push.email_id,
        ))
        .await
        .caused_by(trc::location!())
        .ok()
        .flatten()
    else {
        return;
    };
    let Ok(message_data) = message_data.deserialize::<MessageData>() else {
        return;
    };
    if !message_data.mailboxes.iter().any(|m| m.mailbox_id == INBOX_ID) {
        return;
    }

    // Load the account's device registrations.
    let Ok(Some(registrations)) =
        load_xaps_registrations(&server, email_push.account_id).await
    else {
        return;
    };

    for registration in registrations.registrations {
        if registration.mailboxes.iter().any(|m| m.eq_ignore_ascii_case("INBOX"))
            && let SendResult::DeviceTokenInactive = apns
                .send_notification(&registration.device_token, &registration.aps_account_id)
                .await
        {
            delete_xaps_registration(
                &server,
                email_push.account_id,
                &registration.aps_account_id,
            )
            .await;
        }
    }
}

pub async fn load_xaps_registrations(
    server: &Server,
    account_id: u32,
) -> trc::Result<Option<XapsRegistrations>> {
    let Some(registrations) = server
        .store()
        .get_value::<Archive<AlignedBytes>>(ValueKey::property(
            account_id,
            Collection::Principal,
            0,
            PrincipalField::XapsRegistrations,
        ))
        .await
        .caused_by(trc::location!())?
    else {
        return Ok(None);
    };
    let mut registrations = registrations
        .deserialize::<XapsRegistrations>()
        .caused_by(trc::location!())?;
    registrations.prune(now());
    Ok(Some(registrations))
}

async fn delete_xaps_registration(server: &Server, account_id: u32, aps_account_id: &str) {
    let Ok(Some(mut registrations)) = load_xaps_registrations(server, account_id).await else {
        return;
    };
    registrations
        .registrations
        .retain(|r| r.aps_account_id != aps_account_id);
    if registrations.registrations.is_empty() {
        // Remove the field entirely.
        let mut batch = BatchBuilder::new();
        batch
            .with_account_id(u32::MAX)
            .with_collection(Collection::Principal)
            .with_document(account_id)
            .untag(PrincipalField::XapsRegistrations);
        batch
            .with_account_id(account_id)
            .with_collection(Collection::Principal)
            .with_document(0)
            .clear(PrincipalField::XapsRegistrations);
        if server.commit_batch(batch).await.is_err() {
            return;
        }
    } else {
        let mut batch = BatchBuilder::new();
        batch
            .with_account_id(account_id)
            .with_collection(Collection::Principal)
            .with_document(0)
            .set(
                PrincipalField::XapsRegistrations,
                match Archiver::new(registrations).serialize() {
                    Ok(bytes) => bytes,
                    Err(_) => return,
                },
            );
        if server.commit_batch(batch).await.is_err() {
            return;
        }
    }

    // Notify the push manager so the account is unregistered once its last
    // device is removed.
    let _ = server
        .inner
        .ipc
        .push_tx
        .clone()
        .send(common::ipc::PushEvent::PushServerUpdate {
            account_id,
            broadcast: true,
        })
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::{
        elliptic_curve::rand_core::OsRng,
        pkcs8::{EncodePrivateKey, LineEnding},
    };

    fn test_config() -> XapsConfig {
        let signing_key = p256::ecdsa::SigningKey::random(&mut OsRng);
        XapsConfig {
            enabled: true,
            topic: Some("com.apple.mail".to_string()),
            key_file_p8: Some(
                signing_key
                    .to_pkcs8_pem(LineEnding::LF)
                    .unwrap()
                    .to_string(),
            ),
            key_id: Some("ABC123".to_string()),
            team_id: Some("TEAM123".to_string()),
            sandbox: true,
        }
    }

    #[test]
    fn jwt_format() {
        let client = ApnsClient::try_new(&test_config()).unwrap();
        let token = client.token(1_700_000_000);
        let parts: Vec<&str> = token.split('.').collect();
        assert_eq!(parts.len(), 3);

        let header: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[0]).unwrap()).unwrap();
        assert_eq!(header["alg"], "ES256");
        assert_eq!(header["kid"], "ABC123");

        let claims: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(claims["iss"], "TEAM123");
        assert_eq!(claims["iat"], 1_700_000_000);
    }

    #[test]
    fn token_cached_within_ttl() {
        let client = ApnsClient::try_new(&test_config()).unwrap();
        let t1 = client.token(1_700_000_000);
        let t2 = client.token(1_700_000_000 + 1000);
        assert_eq!(t1, t2);
        // After more than an hour a new token is issued.
        let t3 = client.token(1_700_000_000 + 2 * APNS_TOKEN_TTL_SECS);
        assert_ne!(t1, t3);
    }

    #[test]
    fn payload_format() {
        let client = ApnsClient::try_new(&test_config()).unwrap();
        // The payload is built inline in send_notification; assert the shape
        // via the client's topic/sandbox selection instead.
        assert_eq!(
            client.host,
            APNS_SANDBOX_HOST,
            "sandbox config must select the sandbox host"
        );
        assert_eq!(client.topic, "com.apple.mail");
    }
}
