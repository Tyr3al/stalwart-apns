/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-SEL
 */

#![warn(clippy::large_futures)]

use imap_proto::{ResponseCode, StatusResponse, protocol::capability::Capability};

pub mod core;
pub mod op;

static SERVER_GREETING: &str = "Stalwart IMAP4rev2 at your service.";

/// Builds the greeting, hiding the `XAPPLEPUSHSERVICE` capability when XAPS
/// is not enabled or not fully configured.
pub(crate) fn build_greeting(is_tls: bool, xaps_ready: bool) -> Vec<u8> {
    StatusResponse::ok(SERVER_GREETING)
        .with_code(ResponseCode::Capability {
            capabilities: filter_xaps_capability(
                Capability::all_capabilities(false, is_tls),
                xaps_ready,
            ),
        })
        .into_bytes()
}

/// Removes the `XAPPLEPUSHSERVICE` capability when XAPS is not ready.
#[cfg(feature = "xaps")]
pub(crate) fn filter_xaps_capability(
    mut capabilities: Vec<Capability>,
    xaps_ready: bool,
) -> Vec<Capability> {
    if !xaps_ready {
        capabilities.retain(|c| c != &Capability::XApplePushService);
    }
    capabilities
}

#[cfg(not(feature = "xaps"))]
pub(crate) fn filter_xaps_capability(capabilities: Vec<Capability>, _: bool) -> Vec<Capability> {
    capabilities
}

pub struct ImapError;
