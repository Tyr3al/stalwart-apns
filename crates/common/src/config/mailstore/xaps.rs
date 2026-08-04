/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-SEL
 */

use registry::{
    schema::{prelude::ObjectType, structs::Xaps},
};
use store::registry::bootstrap::Bootstrap;

#[derive(Default, Clone)]
pub struct XapsConfig {
    pub enabled: bool,
    /// APNs topic returned to clients in the XAPPLEPUSHSERVICE reply, must
    /// match the topic of the configured APNs credentials.
    pub topic: Option<String>,
    /// Contents of the APNs authentication key (P8), PKCS#8 PEM encoded
    /// (token-based authentication).
    pub key_file_p8: Option<String>,
    /// Key ID of the APNs authentication key.
    pub key_id: Option<String>,
    /// Team ID of the Apple developer account.
    pub team_id: Option<String>,
    /// Client certificate (PEM) for certificate-based authentication.
    pub certificate_file_pem: Option<String>,
    /// Private key (PEM) for certificate-based authentication.
    pub certificate_file_pem_key: Option<String>,
    /// Base64-encoded PKCS#12 (PFX) certificate and key for
    /// certificate-based authentication.
    pub certificate_file_p12: Option<String>,
    /// Password of the PKCS#12 file (leave empty for password-less files).
    pub certificate_file_p12_password: Option<String>,
    /// Use the APNs sandbox endpoint.
    pub sandbox: bool,
    /// Delay in seconds before non-new-message notifications are sent.
    pub delay: u64,
    /// Interval in seconds between checks for due delayed notifications.
    pub check_interval: u64,
}

impl XapsConfig {
    pub async fn parse(bp: &mut Bootstrap) -> Self {
        let xaps = bp.setting_infallible::<Xaps>().await;
        let key_file_p8 = xaps
            .key_file_p8
            .secret()
            .await
            .map_err(|err| {
                bp.build_error(
                    ObjectType::Xaps.singleton(),
                    format!("Unable to retrieve XAPS key: {err}"),
                );
            })
            .unwrap_or_default()
            .map(|k| k.into_owned());
        let certificate_file_pem = xaps
            .certificate_file_pem
            .secret()
            .await
            .map_err(|err| {
                bp.build_error(
                    ObjectType::Xaps.singleton(),
                    format!("Unable to retrieve XAPS certificate: {err}"),
                );
            })
            .unwrap_or_default()
            .map(|k| k.into_owned());
        let certificate_file_pem_key = xaps
            .certificate_file_pem_key
            .secret()
            .await
            .map_err(|err| {
                bp.build_error(
                    ObjectType::Xaps.singleton(),
                    format!("Unable to retrieve XAPS certificate key: {err}"),
                );
            })
            .unwrap_or_default()
            .map(|k| k.into_owned());
        let certificate_file_p12 = xaps
            .certificate_file_p12
            .secret()
            .await
            .map_err(|err| {
                bp.build_error(
                    ObjectType::Xaps.singleton(),
                    format!("Unable to retrieve XAPS certificate (P12): {err}"),
                );
            })
            .unwrap_or_default()
            .map(|k| k.into_owned());
        let certificate_file_p12_password = xaps
            .certificate_file_p12_password
            .secret()
            .await
            .map_err(|err| {
                bp.build_error(
                    ObjectType::Xaps.singleton(),
                    format!("Unable to retrieve XAPS certificate (P12) password: {err}"),
                );
            })
            .unwrap_or_default()
            .map(|k| k.into_owned());

        // When XAPS is enabled, the APNs topic and one authentication method
        // (token or certificate) are required, otherwise devices would
        // register but never receive push notifications.
        if xaps.enabled {
            if xaps.topic.as_deref().is_none_or(str::is_empty) {
                bp.build_error(
                    ObjectType::Xaps.singleton(),
                    "XAPS is enabled but no APNs topic is configured.",
                );
            }
            let has_token_auth = key_file_p8.as_deref().is_some_and(|k| !k.is_empty());
            let has_cert_auth =
                certificate_file_pem.as_deref().is_some_and(|c| !c.is_empty())
                    && certificate_file_pem_key
                        .as_deref()
                        .is_some_and(|k| !k.is_empty());
            let has_p12_auth =
                certificate_file_p12.as_deref().is_some_and(|c| !c.is_empty());
            if !has_token_auth && !has_cert_auth && !has_p12_auth {
                bp.build_error(
                    ObjectType::Xaps.singleton(),
                    "XAPS is enabled but no APNs authentication method is configured; \
                     configure token (keyFileP8, keyId, teamId), certificate \
                     (certificateFilePem, certificateFilePemKey), or P12 \
                     (certificateFileP12) authentication.",
                );
            }
            if has_token_auth && (xaps.key_id.as_deref().is_none_or(str::is_empty)
                || xaps.team_id.as_deref().is_none_or(str::is_empty))
            {
                bp.build_error(
                    ObjectType::Xaps.singleton(),
                    "XAPS token authentication is configured but keyId and teamId are required.",
                );
            }
        }

        XapsConfig {
            enabled: xaps.enabled,
            topic: xaps.topic,
            key_file_p8,
            key_id: xaps.key_id,
            team_id: xaps.team_id,
            certificate_file_pem,
            certificate_file_pem_key,
            certificate_file_p12,
            certificate_file_p12_password,
            sandbox: xaps.sandbox,
            delay: xaps.delay.max(1),
            check_interval: xaps.check_interval.max(1),
        }
    }
}