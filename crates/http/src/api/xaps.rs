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
//! `<account>` is either a numeric account id or an email address. Requests
//! for the authenticated user's own account are allowed without admin
//! permissions (self-service); other accounts and the list-all endpoint
//! require `SysAccountGet` (list) / `SysAccountUpdate` (delete).

use common::{
    Server,
    auth::AccessToken,
    ipc::PushEvent,
};
use http_proto::{HttpResponse, JsonResponse, ToHttpResponse};
use hyper::Method;
use registry::schema::enums::Permission;
use serde::Serialize;
use services::state_manager::apns::{ApnsClient, SendResult};
use store::{
    Serialize as _, ValueKey,
    write::{AlignedBytes, Archive, Archiver, BatchBuilder, now},
};
use trc::AddContext;
use types::{collection::Collection, field::PrincipalField};

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
        let account_id = resolve_account_id(server, account).await?;
        assert_self_or_admin(access_token, account_id, Permission::SysAccountUpdate)?;

        // The device must be registered for this account.
        let registrations = load_xaps_registrations(server, account_id)
            .await
            .caused_by(trc::location!())?;
        let Some(registration) = registrations
            .into_iter()
            .find(|r| r.aps_account_id == device_id)
        else {
            return Err(trc::ResourceEvent::NotFound.into_err());
        };

        // Report a broken configuration so the UI can surface it.
        let Some(apns) = ApnsClient::try_new(&server.core.xaps) else {
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
                let _ = delete_xaps_registrations(server, account_id, Some(device_id))
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
                let account_id = resolve_account_id(server, account).await?;
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
        let account_id = resolve_account_id(server, account).await?;
        // The account owner may remove their own devices without admin
        // permissions.
        assert_self_or_admin(access_token, account_id, Permission::SysAccountUpdate)?;
        let device_id = path.get(3).copied();
        let removed = delete_xaps_registrations(server, account_id, device_id)
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

/// Resolves an account id from a numeric id or an email address / name.
async fn resolve_account_id(server: &Server, account: &str) -> trc::Result<u32> {
    if let Ok(account_id) = account.parse::<u32>() {
        return Ok(account_id);
    }
    server
        .account_id_from_email(account, false)
        .await
        .caused_by(trc::location!())?
        .ok_or_else(|| trc::ResourceEvent::NotFound.into_err())
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
            device_token: r.device_token,
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
        batch
            .with_account_id(account_id)
            .with_collection(Collection::Principal)
            .with_document(0)
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

#[cfg(test)]
mod tests {
    use super::*;

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
