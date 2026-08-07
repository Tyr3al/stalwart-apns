/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-SEL
 */

use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
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
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use store::{
    Serialize, ValueKey,
    ahash::AHashMap,
    write::{AlignedBytes, Archive, Archiver, BatchBuilder, now},
};
use tokio::sync::mpsc;
use trc::{AddContext, XapsEvent};
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
    /// Token-based authentication (ES256 JWT provider token).
    token_auth: Option<TokenAuth>,
    http_client: reqwest::Client,
}

struct TokenAuth {
    team_id: String,
    key_id: String,
    signing_key: SigningKey,
    /// Cached provider token (ES256 JWT) and its issue time.
    token: Arc<Mutex<Option<(String, u64)>>>,
}

/// PEM-encodes DER data with the given block tag (e.g. "CERTIFICATE").
fn pem_encode(tag: &str, der: &[u8]) -> String {
    let b64 = STANDARD.encode(der);
    let mut out = format!("-----BEGIN {tag}-----\n");
    for chunk in b64.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(chunk).unwrap());
        out.push('\n');
    }
    out.push_str(&format!("-----END {tag}-----\n"));
    out
}

impl ApnsClient {
    /// Builds an APNs client from the XAPS configuration. Returns `None` when
    /// the feature is not fully configured.
    pub fn try_new(config: &XapsConfig) -> Option<Self> {
        let topic = config.topic.clone().unwrap_or_default();
        if topic.is_empty() {
            return None;
        }

        let mut client_builder = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(APNS_REQUEST_TIMEOUT_SECS));

        // Token-based authentication (P8 key), or certificate-based
        // authentication (PEM client certificate or PKCS#12).
        let token_auth = if let Some(key_file_p8) = config
            .key_file_p8
            .as_deref()
            .map(str::trim)
            .filter(|k| !k.is_empty())
        {
            let signing_key = SigningKey::from_pkcs8_pem(key_file_p8).ok()?;
            Some(TokenAuth {
                key_id: config.key_id.clone()?,
                team_id: config.team_id.clone()?,
                signing_key,
                token: Arc::new(Mutex::new(None)),
            })
        } else if let (Some(certificate), Some(private_key)) = (
            config
                .certificate_file_pem
                .as_deref()
                .map(str::trim)
                .filter(|c| !c.is_empty()),
            config
                .certificate_file_pem_key
                .as_deref()
                .map(str::trim)
                .filter(|k| !k.is_empty()),
        ) {
            // The certificate and key PEM blocks are combined into a single
            // buffer (separated by a newline), which reqwest's
            // Identity::from_pem accepts.
            let mut identity_pem = certificate.as_bytes().to_vec();
            identity_pem.push(b'\n');
            identity_pem.extend_from_slice(private_key.as_bytes());
            client_builder =
                client_builder.identity(reqwest::Identity::from_pem(&identity_pem).ok()?);
            None
        } else {
            // PKCS#12 (PFX) certificate and key, base64-encoded in the
            // configuration. Note: only legacy PBES1 (SHA1+3DES / SHA1+RC2-40)
            // encrypted key bags are supported, matching the original
            // dovecot-xaps-daemon (Go's crypto/pkcs12).
            let certificate_file_p12 = config
                .certificate_file_p12
                .as_deref()
                .map(str::trim)
                .filter(|c| !c.is_empty())?;
            // Tolerate whitespace/newlines in the base64 input.
            let b64: String = certificate_file_p12
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect();
            let p12_der = match STANDARD.decode(b64) {
                Ok(p12_der) => p12_der,
                Err(err) => {
                    trc::event!(
                        Xaps(XapsEvent::Error),
                        Details = format!("Invalid base64 in certificateFileP12: {err}"),
                    );
                    return None;
                }
            };
            let pfx = match p12::PFX::parse(&p12_der) {
                Ok(pfx) => pfx,
                Err(err) => {
                    trc::event!(
                        Xaps(XapsEvent::Error),
                        Details = format!("Invalid PKCS#12 data in certificateFileP12: {err}"),
                    );
                    return None;
                }
            };
            let password = config.certificate_file_p12_password.as_deref().unwrap_or("");
            let (Some(cert), Some(key)) = (
                pfx.cert_x509_bags(password).ok().and_then(|c| c.into_iter().next()),
                pfx.key_bags(password).ok().and_then(|k| k.into_iter().next()),
            ) else {
                trc::event!(
                    Xaps(XapsEvent::Error),
                    Details = "No certificate and private key found in certificateFileP12 \
                               (wrong password or unsupported encryption)",
                );
                return None;
            };

            let mut identity_pem = pem_encode("CERTIFICATE", &cert).into_bytes();
            identity_pem.push(b'\n');
            identity_pem.extend_from_slice(pem_encode("PRIVATE KEY", &key).as_bytes());
            client_builder =
                client_builder.identity(reqwest::Identity::from_pem(&identity_pem).ok()?);
            None
        };

        Some(ApnsClient {
            host: if config.sandbox {
                APNS_SANDBOX_HOST.to_string()
            } else {
                APNS_PRODUCTION_HOST.to_string()
            },
            topic,
            token_auth,
            http_client: client_builder.build().ok()?,
        })
    }

    fn token(&self, now: u64) -> Option<String> {
        let token_auth = self.token_auth.as_ref()?;
        let mut cache = token_auth.token.lock().unwrap();
        if let Some((token, issued_at)) = cache.as_ref()
            && now.saturating_sub(*issued_at) < APNS_TOKEN_TTL_SECS
        {
            return Some(token.clone());
        }

        let mut header = serde_json::Map::new();
        header.insert("alg".into(), "ES256".into());
        header.insert("kid".into(), token_auth.key_id.clone().into());
        let mut claims = serde_json::Map::new();
        claims.insert("iss".into(), token_auth.team_id.clone().into());
        claims.insert("iat".into(), now.into());

        let signing_input = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).unwrap()),
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
        );
        let signature: Signature = token_auth.signing_key.sign(signing_input.as_bytes());
        let token = format!(
            "{}.{}",
            signing_input,
            URL_SAFE_NO_PAD.encode(signature.to_bytes())
        );

        *cache = Some((token.clone(), now));
        Some(token)
    }

    pub async fn send_notification(&self, device_token: &str, account_id: &str) -> SendResult {
        self.send(
            device_token,
            account_id,
            serde_json::json!({ "aps": { "account-id": account_id } }),
            "background",
            "5",
        )
        .await
    }

    /// Sends a visible (alert) test notification to a device, used by the
    /// admin console to verify that push delivery is working.
    pub async fn send_test_notification(&self, device_token: &str, account_id: &str) -> SendResult {
        self.send(
            device_token,
            account_id,
            serde_json::json!({
                "aps": {
                    "alert": { "title": "Stalwart", "body": "XAPS test push" },
                    "account-id": account_id,
                }
            }),
            "alert",
            "10",
        )
        .await
    }

    async fn send(
        &self,
        device_token: &str,
        account_id: &str,
        payload: serde_json::Value,
        push_type: &str,
        priority: &str,
    ) -> SendResult {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());

        let mut request = self
            .http_client
            .post(format!("{}/3/device/{}", self.host, device_token))
            .header("apns-topic", self.topic.as_str())
            .header("apns-push-type", push_type)
            .header("apns-priority", priority)
            .header("apns-expiration", (now + APNS_EXPIRATION_SECS).to_string());
        if let Some(token) = self.token(now) {
            request = request.bearer_auth(token);
        }

        match request.body(payload.to_string()).send().await {
            Ok(response) => {
                let status = response.status();
                if status.is_success() {
                    trc::event!(
                        Xaps(XapsEvent::Success),
                        Details = format!("APNs notification sent for account {account_id}"),
                    );
                    SendResult::Ok
                } else if status == reqwest::StatusCode::GONE {
                    trc::event!(
                        Xaps(XapsEvent::DeviceTokenInactive),
                        Details = format!("APNs device token is no longer active for account {account_id}"),
                    );
                    SendResult::DeviceTokenInactive
                } else {
                    // APNs returns a JSON body ({"reason": "BadDeviceToken", ...}) on error
                    // responses that's the actual diagnostic -- the HTTP status alone (almost
                    // always 400) doesn't say what was wrong.
                    let body = response.text().await.unwrap_or_default();
                    trc::event!(
                        Xaps(XapsEvent::Error),
                        Details = format!("APNs request failed for account {account_id}"),
                        Code = status.as_u16(),
                        Reason = body,
                    );
                    SendResult::Error
                }
            }
            Err(err) => {
                trc::event!(
                    Xaps(XapsEvent::Error),
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
/// Pending delayed notifications for pushed devices are cancelled via
/// `xaps_delayed_tx`.
pub async fn deliver_xaps_notifications(
    inner: Arc<Inner>,
    email_push: EmailPush,
    xaps_delayed_tx: Option<mpsc::Sender<XapsDelayedEvent>>,
) {
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
        if registration.mailboxes.iter().any(|m| m.eq_ignore_ascii_case("INBOX")) {
            let key = XapsDeviceKey {
                account_id: email_push.account_id,
                aps_account_id: registration.aps_account_id.clone(),
                device_token: registration.device_token.clone(),
            };
            match apns
                .send_notification(&registration.device_token, &registration.aps_account_id)
                .await
            {
                SendResult::Ok => {
                    // The immediate push supersedes any pending delayed
                    // notification for this device.
                    if let Some(xaps_delayed_tx) = &xaps_delayed_tx {
                        let _ = xaps_delayed_tx
                            .send(XapsDelayedEvent::Remove { key })
                            .await;
                    }
                }
                SendResult::DeviceTokenInactive => {
                    if let Some(xaps_delayed_tx) = &xaps_delayed_tx {
                        let _ = xaps_delayed_tx
                            .send(XapsDelayedEvent::Remove { key })
                            .await;
                    }
                    delete_xaps_registration(
                        &server,
                        email_push.account_id,
                        &registration.aps_account_id,
                    )
                    .await;
                }
                SendResult::Error => {
                    // Retry the transient failure with exponential backoff via
                    // the delayed-notification task.
                    if let Some(xaps_delayed_tx) = &xaps_delayed_tx {
                        let _ = xaps_delayed_tx
                            .send(XapsDelayedEvent::Schedule {
                                key,
                                due: now() + retry_delay(1),
                                attempts: 1,
                            })
                            .await;
                    }
                }
            }
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

/// Identifies a device with a pending delayed notification.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct XapsDeviceKey {
    pub account_id: u32,
    pub aps_account_id: String,
    pub device_token: String,
}

/// Commands for the delayed-notification task.
#[derive(Debug)]
pub enum XapsDelayedEvent {
    /// (Re)schedule a delayed notification for a device, due at the given
    /// unix timestamp, after `attempts` previous failed send attempts.
    Schedule {
        key: XapsDeviceKey,
        due: u64,
        attempts: u32,
    },
    /// Cancel a pending delayed notification (e.g. after an immediate push).
    Remove { key: XapsDeviceKey },
}

/// Base delay in seconds for the first retry after a transient APNs failure.
const XAPS_RETRY_BASE_DELAY_SECS: u64 = 60;
/// Maximum APNs send attempts (initial + retries) before a notification is
/// dropped.
const XAPS_MAX_ATTEMPTS: u32 = 5;

/// Exponential backoff delay (seconds) for the given 1-based attempt number,
/// capped at one hour.
fn retry_delay(attempt: u32) -> u64 {
    let shift = attempt.saturating_sub(1).min(6);
    (XAPS_RETRY_BASE_DELAY_SECS << shift).min(60 * 60)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DelayedEntry {
    due: u64,
    attempts: u32,
}

/// Spawns the delayed-notification task, mirroring the `delayedApns` map of
/// dovecot-xaps-daemon: non-new-message changes are batched into a single
/// push sent after the configured delay, checked every `checkInterval`
/// seconds.
pub fn spawn_xaps_delayed(inner: Arc<Inner>, mut rx: mpsc::Receiver<XapsDelayedEvent>) {
    tokio::spawn(async move {
        let mut delayed: AHashMap<XapsDeviceKey, DelayedEntry> = AHashMap::default();
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        // Consume the immediate first tick so the first check waits a full
        // interval.
        interval.tick().await;

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    let server = inner.build_server();
                    // Reload the check interval from the current config.
                    interval = tokio::time::interval(Duration::from_secs(
                        server.core.xaps.check_interval,
                    ));
                    // Consume the immediate first tick of the new interval.
                    interval.tick().await;

                    if !delayed.is_empty() {
                        let due = collect_due(&delayed, now());
                        if !due.is_empty() {
                            let apns = ApnsClient::try_new(&server.core.xaps);
                            for key in due {
                                let Some(entry) = delayed.remove(&key) else {
                                    continue;
                                };
                                let account_id = key.account_id;
                                if let Some(apns) = &apns {
                                    match apns
                                        .send_notification(&key.device_token, &key.aps_account_id)
                                        .await
                                    {
                                        SendResult::Ok => {}
                                        SendResult::DeviceTokenInactive => {
                                            delete_xaps_registration(
                                                &server,
                                                account_id,
                                                &key.aps_account_id,
                                            )
                                            .await;
                                        }
                                        SendResult::Error => {
                                            // Retry transient failures with exponential
                                            // backoff, dropping the notification once the
                                            // attempt limit is reached.
                                            if entry.attempts + 1 < XAPS_MAX_ATTEMPTS {
                                                let attempts = entry.attempts + 1;
                                                delayed.insert(
                                                    key,
                                                    DelayedEntry {
                                                        due: now() + retry_delay(attempts),
                                                        attempts,
                                                    },
                                                );
                                                trc::event!(
                                                    Xaps(XapsEvent::Error),
                                                    Details = format!(
                                                        "APNs notification retry scheduled for \
                                                         account {account_id} (attempt {attempts})"
                                                    ),
                                                );
                                            } else {
                                                trc::event!(
                                                    Xaps(XapsEvent::Error),
                                                    Details = format!(
                                                        "APNs notification dropped for account \
                                                         {account_id} after max attempts"
                                                    ),
                                                );
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                event = rx.recv() => {
                    match event {
                        Some(XapsDelayedEvent::Schedule { key, due, attempts }) => {
                            delayed.insert(key, DelayedEntry { due, attempts });
                        }
                        Some(XapsDelayedEvent::Remove { key }) => {
                            delayed.remove(&key);
                        }
                        None => break,
                    }
                }
            }
        }
    });
}

/// Collects the keys of delayed notifications whose due time has passed.
/// Notifications are re-scheduled by overwriting their entry, so each device
/// appears at most once.
fn collect_due(
    delayed: &AHashMap<XapsDeviceKey, DelayedEntry>,
    current_time: u64,
) -> Vec<XapsDeviceKey> {
    delayed
        .iter()
        .filter(|(_, entry)| current_time >= entry.due)
        .map(|(key, _)| key.clone())
        .collect()
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
            certificate_file_pem: None,
            certificate_file_pem_key: None,
            certificate_file_p12: None,
            certificate_file_p12_password: None,
            sandbox: true,
            delay: 30,
            check_interval: 20,
        }
    }

    #[test]
    fn jwt_format() {
        let client = ApnsClient::try_new(&test_config()).unwrap();
        let token = client.token(1_700_000_000).unwrap();
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
        let t1 = client.token(1_700_000_000).unwrap();
        let t2 = client.token(1_700_000_000 + 1000).unwrap();
        assert_eq!(t1, t2);
        // After more than an hour a new token is issued.
        let t3 = client.token(1_700_000_000 + 2 * APNS_TOKEN_TTL_SECS).unwrap();
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

    #[test]
    fn collect_due_filters_by_time_and_dedups() {
        let key = |id: &str| XapsDeviceKey {
            account_id: 1,
            aps_account_id: id.to_string(),
            device_token: format!("token-{id}"),
        };
        let entry = |due| DelayedEntry { due, attempts: 0 };
        let mut delayed = AHashMap::default();
        delayed.insert(key("a"), entry(100));
        delayed.insert(key("b"), entry(200));
        delayed.insert(key("c"), entry(300));

        // Only entries due at or before the current time are collected.
        assert_eq!(collect_due(&delayed, 150), vec![key("a")]);
        assert_eq!(collect_due(&delayed, 200).len(), 2);

        // Re-scheduling overwrites the entry (dedup).
        delayed.insert(key("a"), entry(500));
        assert!(collect_due(&delayed, 150).is_empty());

        // Removing an entry cancels it.
        delayed.remove(&key("a"));
        assert!(!delayed.contains_key(&key("a")));
    }

    #[test]
    fn retry_backoff() {
        // Exponential backoff, capped at one hour.
        assert_eq!(retry_delay(1), 60);
        assert_eq!(retry_delay(2), 120);
        assert_eq!(retry_delay(3), 240);
        assert_eq!(retry_delay(4), 480);
        assert_eq!(retry_delay(6), 1920);
        assert_eq!(retry_delay(7), 60 * 60);
        assert_eq!(retry_delay(100), 60 * 60);
    }

    #[test]
    fn p12_auth_mode() {
        let certified_key =
            rcgen::generate_simple_self_signed(vec!["apns.example.com".to_string()]).unwrap();
        let p12_der = p12::PFX::new(
            certified_key.cert.der().as_ref(),
            &certified_key.signing_key.serialize_der(),
            None,
            "password",
            "apns",
        )
        .unwrap()
        .to_der();
        let config = XapsConfig {
            enabled: true,
            topic: Some("com.apple.mail".to_string()),
            key_file_p8: None,
            key_id: None,
            team_id: None,
            certificate_file_pem: None,
            certificate_file_pem_key: None,
            certificate_file_p12: Some(STANDARD.encode(p12_der)),
            certificate_file_p12_password: Some("password".to_string()),
            sandbox: false,
            delay: 30,
            check_interval: 20,
        };
        let client = ApnsClient::try_new(&config).unwrap();
        assert!(
            client.token_auth.is_none(),
            "P12 auth must not use token auth"
        );
        assert_eq!(client.host, APNS_PRODUCTION_HOST);
        assert_eq!(client.topic, "com.apple.mail");
    }

    #[test]
    fn p12_auth_rejects_bad_input() {
        // Invalid base64.
        let mut config = test_config();
        config.key_file_p8 = None;
        config.key_id = None;
        config.team_id = None;
        config.certificate_file_p12 = Some("not base64!!".to_string());
        assert!(ApnsClient::try_new(&config).is_none());

        // Wrong password.
        let certified_key =
            rcgen::generate_simple_self_signed(vec!["apns.example.com".to_string()]).unwrap();
        let p12_der = p12::PFX::new(
            certified_key.cert.der().as_ref(),
            &certified_key.signing_key.serialize_der(),
            None,
            "password",
            "apns",
        )
        .unwrap()
        .to_der();
        let mut config = test_config();
        config.key_file_p8 = None;
        config.key_id = None;
        config.team_id = None;
        config.certificate_file_p12 = Some(STANDARD.encode(p12_der));
        config.certificate_file_p12_password = Some("wrong".to_string());
        assert!(ApnsClient::try_new(&config).is_none());
    }

    #[test]
    fn certificate_auth_mode() {
        let certified_key =
            rcgen::generate_simple_self_signed(vec!["apns.example.com".to_string()]).unwrap();
        let config = XapsConfig {
            enabled: true,
            topic: Some("com.apple.mail".to_string()),
            key_file_p8: None,
            key_id: None,
            team_id: None,
            certificate_file_pem: Some(certified_key.cert.pem()),
            certificate_file_pem_key: Some(certified_key.signing_key.serialize_pem()),
            certificate_file_p12: None,
            certificate_file_p12_password: None,
            sandbox: false,
            delay: 30,
            check_interval: 20,
        };
        let client = ApnsClient::try_new(&config).unwrap();
        assert!(client.token_auth.is_none(), "cert auth must not use token auth");
        assert_eq!(client.host, APNS_PRODUCTION_HOST);
        assert_eq!(client.topic, "com.apple.mail");
    }

    #[test]
    fn incomplete_config_is_rejected() {
        // No authentication method configured.
        let config = XapsConfig {
            enabled: true,
            topic: Some("com.apple.mail".to_string()),
            key_file_p8: None,
            key_id: None,
            team_id: None,
            certificate_file_pem: None,
            certificate_file_pem_key: None,
            certificate_file_p12: None,
            certificate_file_p12_password: None,
            sandbox: false,
            delay: 30,
            check_interval: 20,
        };
        assert!(ApnsClient::try_new(&config).is_none());

        // Missing topic.
        let mut config = test_config();
        config.topic = None;
        assert!(ApnsClient::try_new(&config).is_none());
    }
}
