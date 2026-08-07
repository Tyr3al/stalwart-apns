/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-SEL
 */

//! Management API for XAPS device registrations, consumed by the web admin
//! console and by users managing their own devices:
//!
//! - `GET    /api/xaps/registrations`                      list accounts with registered devices (admin)
//! - `GET    /api/xaps/registrations/<account>`            list one account's devices (admin or self)
//! - `DELETE /api/xaps/registrations/<account>`            remove all devices of an account (admin or self)
//! - `DELETE /api/xaps/registrations/<account>/<apsAccountId>` remove one device (admin or self)
//! - `POST   /api/xaps/test/<account>/<apsAccountId>`      send a test push to one device (admin or self)
//!
//! `<account>` is either a numeric account id, a JMAP account id, or an email
//! address. Requests for the authenticated user's own account are allowed
//! without admin permissions (self-service); other accounts and the list-all
//! endpoint require `SysAccountGet` (list) / `SysAccountUpdate` (delete).

use common::{
    KV_RATE_LIMIT_XAPS, Server,
    auth::AccessToken,
    ipc::PushEvent,
};
use http_proto::{HttpResponse, JsonResponse, ToHttpResponse};
use hyper::Method;
use percent_encoding::percent_decode_str;
use registry::schema::{enums::Permission, prelude::Duration, structs::Rate};
use serde::Serialize;
use services::state_manager::apns::{
    ApnsClient, SendResult, load_xaps_registrations as load_xaps_registrations_unmasked,
};
use store::{
    Serialize as _, ValueKey,
    write::{AlignedBytes, Archive, Archiver, BatchBuilder, now},
};
use trc::{AddContext, LimitEvent};
use types::{collection::Collection, field::PrincipalField, id::Id};

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct XapsRegistration {
    pub aps_account_id: String,
    pub device_token: String,
    pub mailboxes: Vec<String>,
    pub registered_at: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct XapsAccount {
    pub account_id: u32,
    pub account_name: String,
    pub registrations: Vec<XapsRegistration>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct XapsAccounts {
    pub accounts: Vec<XapsAccount>,
}

#[derive(Debug, Serialize)]
pub struct TestResult {
    pub status: &'static str,
}

pub async fn handle_xaps_api_request(
    server: &Server,
    path: &[&str],
    access_token: &AccessToken,
    method: &Method,
) -> trc::Result<HttpResponse> {
    // Routes are under /api/xaps/registrations/... and /api/xaps/test/...
    if method == Method::POST && path.get(1).copied() == Some("test") {
        // POST /api/xaps/test/<account>/<apsAccountId> — send a test push.
        let Some(account) = path.get(2).copied() else {
            return Err(trc::ResourceEvent::NotFound.into_err());
        };
        let Some(device_id) = path.get(3).copied() else {
            return Err(trc::ResourceEvent::NotFound.into_err());
        };
        let account = percent_decode_str(account)
            .decode_utf8_lossy()
            .into_owned();
        let device_id = percent_decode_str(device_id)
            .decode_utf8_lossy()
            .into_owned();
        let account_id = resolve_account_id(server, &account).await?;
        assert_self_or_admin(access_token, account_id, Permission::SysAccountUpdate)?;

        // Limit test pushes to 10 per minute per account.
        if server
            .in_memory_store()
            .is_rate_allowed(
                KV_RATE_LIMIT_XAPS,
                &account_id.to_be_bytes(),
                &Rate {
                    count: 10,
                    period: Duration::from_millis(60_000),
                },
                false,
            )
            .await?
            .is_some()
        {
            return Err(LimitEvent::TooManyRequests.into_err());
        }

        // The device must be registered for this account. Uses the unmasked
        // loader (same one the real push-delivery path uses) rather than
        // this file's own load_xaps_registrations, which masks the device
        // token for the public registrations list -- sending that masked
        // placeholder to APNs as a "device token" always fails as
        // BadDeviceToken, regardless of how valid the real token is.
        let registrations = load_xaps_registrations_unmasked(server, account_id)
            .await
            .caused_by(trc::location!())?;
        let Some(registration) = registrations
            .into_iter()
            .flat_map(|r| r.registrations)
            .find(|r| r.aps_account_id == device_id)
        else {
            return Err(trc::ResourceEvent::NotFound.into_err());
        };

        // Report a broken configuration so the UI can surface it.
        let Some(apns) = ApnsClient::get_cached(&server.core.xaps) else {
            return Ok(JsonResponse::new(TestResult {
                status: "not-configured",
            })
            .into_http_response());
        };

        match apns
            .send_test_notification(&registration.device_token, &registration.aps_account_id)
            .await
        {
            SendResult::Ok => Ok(JsonResponse::new(TestResult { status: "ok" }).into_http_response()),
            SendResult::DeviceTokenInactive => {
                // The device is no longer valid, remove its registration.
                let _ = delete_xaps_registrations(server, account_id, Some(device_id.as_str()))
                    .await
                    .caused_by(trc::location!())?;
                Ok(JsonResponse::new(TestResult {
                    status: "device-inactive",
                })
                .into_http_response())
            }
            SendResult::Error => Ok(JsonResponse::new(TestResult {
                status: "error",
            })
            .into_http_response()),
        }
    } else if path.get(1).copied() != Some("registrations") {
        Err(trc::ResourceEvent::NotFound.into_err())
    } else if method == Method::GET {
        match path.get(2).copied() {
            // List all accounts with registered devices (admin only).
            None => {
                access_token.enforce_permission(Permission::SysAccountGet)?;
                let mut accounts = Vec::new();
                for account_id in server
                    .document_ids(
                        u32::MAX,
                        Collection::Principal,
                        PrincipalField::XapsRegistrations,
                    )
                    .await?
                {
                    // Skip accounts that were deleted since the scan.
                    if let Some(account) = load_xaps_account(server, account_id)
                        .await
                        .caused_by(trc::location!())?
                    {
                        accounts.push(account);
                    }
                }
                Ok(JsonResponse::new(XapsAccounts { accounts }).into_http_response())
            }
            // List one account's devices (admin or the account owner).
            Some(account) => {
                let account = percent_decode_str(account)
                    .decode_utf8_lossy()
                    .into_owned();
                let account_id = resolve_account_id(server, &account).await?;
                assert_self_or_admin(access_token, account_id, Permission::SysAccountGet)?;
                Ok(JsonResponse::new(
                    load_xaps_account(server, account_id)
                        .await
                        .caused_by(trc::location!())?
                        .ok_or_else(|| trc::ResourceEvent::NotFound.into_err())?,
                )
                .into_http_response())
            }
        }
    } else if method == Method::DELETE {
        let Some(account) = path.get(2).copied() else {
            return Err(trc::ResourceEvent::NotFound.into_err());
        };
        let account = percent_decode_str(account)
            .decode_utf8_lossy()
            .into_owned();
        let account_id = resolve_account_id(server, &account).await?;
        // The account owner may remove their own devices without admin
        // permissions.
        assert_self_or_admin(access_token, account_id, Permission::SysAccountUpdate)?;
        let device_id = path
            .get(3)
            .copied()
            .map(|device_id| percent_decode_str(device_id).decode_utf8_lossy().into_owned());
        let removed = delete_xaps_registrations(server, account_id, device_id.as_deref())
            .await
            .caused_by(trc::location!())?;
        if removed {
            Ok(JsonResponse::new(()).into_http_response())
        } else {
            Err(trc::ResourceEvent::NotFound.into_err())
        }
    } else {
        Err(trc::ResourceEvent::NotFound.into_err())
    }
}

/// Allows the request when the target account is the authenticated user's own
/// account, otherwise requires the given admin permission.
fn assert_self_or_admin(
    access_token: &AccessToken,
    account_id: u32,
    admin_permission: Permission,
) -> trc::Result<()> {
    if access_token.account_id() == account_id {
        Ok(())
    } else {
        access_token.enforce_permission(admin_permission)
    }
}

/// Resolves an account id from a numeric id, JMAP account id, or email address.
async fn resolve_account_id(server: &Server, account: &str) -> trc::Result<u32> {
    if let Some(account_id) = parse_account_id(account) {
        return Ok(account_id);
    }
    server
        .account_id_from_email(account, false)
        .await
        .caused_by(trc::location!())?
        .ok_or_else(|| trc::ResourceEvent::NotFound.into_err())
}

/// Parses the two account-id representations exposed by Stalwart APIs.
///
/// The management API uses decimal document ids, while the JMAP session used
/// by the account WebUI exposes the same id using Stalwart's canonical base32
/// representation. Only canonical, unprefixed JMAP ids are accepted so an
/// arbitrary account name is not silently interpreted as an id.
fn parse_account_id(account: &str) -> Option<u32> {
    account.parse::<u32>().ok().or_else(|| {
        let id = account.parse::<Id>().ok()?;
        (id.prefix_id() == 0 && id.as_string() == account).then(|| id.document_id())
    })
}

async fn load_xaps_account(
    server: &Server,
    account_id: u32,
) -> trc::Result<Option<XapsAccount>> {
    let Some(account_cache) = server
        .try_account(account_id)
        .await
        .caused_by(trc::location!())?
    else {
        return Ok(None);
    };
    let registrations = load_xaps_registrations(server, account_id)
        .await
        .caused_by(trc::location!())?;
    Ok(Some(XapsAccount {
        account_id,
        account_name: account_cache.name().to_string(),
        registrations,
    }))
}

async fn load_xaps_registrations(server: &Server, account_id: u32) -> trc::Result<Vec<XapsRegistration>> {
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
        return Ok(Vec::new());
    };
    let mut registrations = registrations
        .deserialize::<email::push::xaps::XapsRegistrations>()
        .caused_by(trc::location!())?;
    // Drop stale registrations so the admin view matches what the push
    // manager uses.
    registrations.prune(now());

    Ok(registrations
        .registrations
        .into_iter()
        .map(|r| XapsRegistration {
            aps_account_id: r.aps_account_id,
            device_token: mask_token(&r.device_token),
            mailboxes: r.mailboxes,
            registered_at: r.registered_at,
        })
        .collect())
}

/// Removes all (or one) devices of an account. Returns true when something
/// was removed.
async fn delete_xaps_registrations(
    server: &Server,
    account_id: u32,
    device_id: Option<&str>,
) -> trc::Result<bool> {
    let Some(registrations_archive) = server
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
        return Ok(false);
    };
    let mut registrations = registrations_archive
        .deserialize::<email::push::xaps::XapsRegistrations>()
        .caused_by(trc::location!())?;

    let removed = if let Some(device_id) = device_id {
        let before = registrations.registrations.len();
        registrations
            .registrations
            .retain(|r| r.aps_account_id != device_id);
        registrations.registrations.len() != before
    } else {
        let removed = !registrations.registrations.is_empty();
        registrations.registrations.clear();
        removed
    };
    if !removed {
        return Ok(false);
    }

    let mut batch = BatchBuilder::new();
    if registrations.registrations.is_empty() {
        batch
            .with_account_id(u32::MAX)
            .with_collection(Collection::Principal)
            .with_document(account_id)
            .untag(PrincipalField::XapsRegistrations);
        // Guard against a concurrent registration upsert.
        batch
            .with_account_id(account_id)
            .with_collection(Collection::Principal)
            .with_document(0)
            .assert_value(PrincipalField::XapsRegistrations, registrations_archive)
            .clear(PrincipalField::XapsRegistrations);
    } else {
        // Guard against a concurrent registration upsert.
        batch
            .with_account_id(account_id)
            .with_collection(Collection::Principal)
            .with_document(0)
            .assert_value(PrincipalField::XapsRegistrations, registrations_archive)
            .set(
                PrincipalField::XapsRegistrations,
                Archiver::new(registrations)
                    .serialize()
                    .caused_by(trc::location!())?,
            );
    }
    server
        .commit_batch(batch)
        .await
        .caused_by(trc::location!())?;

    // Notify the push manager so the account's devices are re-synced and
    // unregistered once the last one is removed.
    let _ = server
        .inner
        .ipc
        .push_tx
        .clone()
        .send(PushEvent::PushServerUpdate {
            account_id,
            broadcast: true,
        })
        .await;

    Ok(true)
}

/// Masks a device token so the full push credential is never exposed in API
/// responses or the admin console, consistent with how other secrets are
/// handled (e.g. MASKED_PASSWORD).
///
/// Valid device tokens are ASCII hex (enforced by the IMAP parser), but this
/// masks by `char`s rather than bytes so it can never panic on a token that
/// predates that validation and contains multi-byte UTF-8 -- slicing by byte
/// offset would panic if the offset landed inside a multi-byte character.
fn mask_token(token: &str) -> String {
    let chars: Vec<char> = token.chars().collect();
    if chars.len() <= 8 {
        "****".to_string()
    } else {
        let prefix: String = chars[..4].iter().collect();
        let suffix: String = chars[chars.len() - 4..].iter().collect();
        format!("{prefix}****{suffix}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_token_shape() {
        assert_eq!(mask_token("1234"), "****");
        assert_eq!(mask_token("12345678"), "****");
        assert_eq!(mask_token("1234567890abcdef"), "1234****cdef");
    }

    #[test]
    fn mask_token_multibyte_utf8_does_not_panic() {
        // Tokens registered before the IMAP parser started rejecting
        // non-hex device tokens may contain arbitrary UTF-8, including
        // multi-byte characters that straddle the byte offsets a naive
        // `&token[..4]` slice would use.
        assert_eq!(mask_token("abcé0123456789"), "abcé****6789");
        // A string of 4-byte emoji: short enough to hit the "****" branch
        // (<= 8 chars) even though it is far more than 8 bytes.
        assert_eq!(mask_token("😀😀😀😀😀😀😀😀"), "****");
        // Longer than 8 chars: must slice on char boundaries without
        // panicking and must not split any individual emoji.
        assert_eq!(
            mask_token("😀😀😀😀😀😀😀😀😀😀"),
            "😀😀😀😀****😀😀😀😀"
        );
    }

    #[test]
    fn parses_management_and_jmap_account_ids() {
        assert_eq!(parse_account_id("42"), Some(42));

        let jmap_id = Id::from(42_u32).as_string();
        assert_eq!(parse_account_id(&jmap_id), Some(42));

        assert_eq!(parse_account_id("A"), None);
        assert_eq!(parse_account_id("a@example.org"), None);
        assert_eq!(parse_account_id("abca"), None);
        assert_eq!(
            parse_account_id(&Id::from_parts(1, 42).as_string()),
            None
        );
    }

    #[test]
    fn serialization_shape() {
        // The JSON field names are the API contract consumed by the web admin
        // console.
        let account = XapsAccount {
            account_id: 1,
            account_name: "user@example.org".to_string(),
            registrations: vec![XapsRegistration {
                aps_account_id: "uuid".to_string(),
                device_token: "token".to_string(),
                mailboxes: vec!["INBOX".to_string()],
                registered_at: 123,
            }],
        };
        let json = serde_json::to_value(account).unwrap();
        assert_eq!(json["accountId"], 1);
        assert_eq!(json["accountName"], "user@example.org");
        assert_eq!(json["registrations"][0]["apsAccountId"], "uuid");
        assert_eq!(json["registrations"][0]["deviceToken"], "token");
        assert_eq!(json["registrations"][0]["mailboxes"][0], "INBOX");
        assert_eq!(json["registrations"][0]["registeredAt"], 123);
    }
}
