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
    /// Contents of the APNs authentication key (P8), PKCS#8 PEM encoded.
    pub key_file_p8: Option<String>,
    /// Key ID of the APNs authentication key.
    pub key_id: Option<String>,
    /// Team ID of the Apple developer account.
    pub team_id: Option<String>,
    /// Use the APNs sandbox endpoint.
    pub sandbox: bool,
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

        // When XAPS is enabled, all APNs credentials are required, otherwise
        // devices would register but never receive push notifications.
        if xaps.enabled {
            if xaps.topic.as_deref().is_none_or(str::is_empty) {
                bp.build_error(
                    ObjectType::Xaps.singleton(),
                    "XAPS is enabled but no APNs topic is configured.",
                );
            }
            if key_file_p8.as_deref().is_none_or(str::is_empty) {
                bp.build_error(
                    ObjectType::Xaps.singleton(),
                    "XAPS is enabled but no APNs authentication key (keyFileP8) is configured.",
                );
            }
            if xaps.key_id.as_deref().is_none_or(str::is_empty) {
                bp.build_error(
                    ObjectType::Xaps.singleton(),
                    "XAPS is enabled but no APNs key ID is configured.",
                );
            }
            if xaps.team_id.as_deref().is_none_or(str::is_empty) {
                bp.build_error(
                    ObjectType::Xaps.singleton(),
                    "XAPS is enabled but no APNs team ID is configured.",
                );
            }
        }

        XapsConfig {
            enabled: xaps.enabled,
            topic: xaps.topic,
            key_file_p8,
            key_id: xaps.key_id,
            team_id: xaps.team_id,
            sandbox: xaps.sandbox,
        }
    }
}