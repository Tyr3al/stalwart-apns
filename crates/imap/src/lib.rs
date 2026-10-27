/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-SEL
 */

#![warn(clippy::large_futures)]

use common::Server;
use imap_proto::{
    ResponseCode, StatusResponse,
    protocol::capability::{AuthCapability, Capability},
};
use std::sync::LazyLock;

pub mod core;
pub mod op;

static SERVER_GREETING: &str = "Stalwart IMAP4rev2 at your service.";

const AUTH_CAPABILITIES: [AuthCapability; 3] = [
    AuthCapability::Disabled,
    AuthCapability::Plain,
    AuthCapability::PlainAndBearer,
];

static GREETINGS: LazyLock<[[Vec<u8>; 3]; 2]> = LazyLock::new(|| {
    [false, true].map(|offer_tls| AUTH_CAPABILITIES.map(|auth| build_greeting(offer_tls, auth)))
});

pub(crate) fn greeting(
    offer_tls: bool,
    auth: AuthCapability,
    xaps_ready: bool,
) -> std::borrow::Cow<'static, [u8]> {
    if xaps_ready {
        // The cached greetings never advertise XAPPLEPUSHSERVICE, as it is only offered
        // once XAPS is enabled and fully configured.
        StatusResponse::ok(SERVER_GREETING)
            .with_code(ResponseCode::Capability {
                capabilities: with_xaps_capability(
                    Capability::all_capabilities(false, offer_tls, auth, 0, 0),
                    true,
                ),
            })
            .into_bytes()
            .into()
    } else {
        GREETINGS[usize::from(offer_tls)][auth as usize]
            .as_slice()
            .into()
    }
}

/// Adds the `XAPPLEPUSHSERVICE` capability when XAPS is enabled and fully configured.
#[cfg(feature = "xaps")]
pub(crate) fn with_xaps_capability(
    mut capabilities: Vec<Capability>,
    xaps_ready: bool,
) -> Vec<Capability> {
    if xaps_ready {
        capabilities.push(Capability::XApplePushService);
    }
    capabilities
}

#[cfg(not(feature = "xaps"))]
pub(crate) fn with_xaps_capability(capabilities: Vec<Capability>, _: bool) -> Vec<Capability> {
    capabilities
}

pub(crate) fn auth_capability(server: &Server, is_tls: bool) -> AuthCapability {
    if !is_tls && !server.core.imap.allow_plain_auth {
        AuthCapability::Disabled
    } else if server.accepts_bearer_tokens(is_tls) {
        AuthCapability::PlainAndBearer
    } else {
        AuthCapability::Plain
    }
}

fn build_greeting(offer_tls: bool, auth: AuthCapability) -> Vec<u8> {
    StatusResponse::ok(SERVER_GREETING)
        .with_code(ResponseCode::Capability {
            capabilities: Capability::all_capabilities(false, offer_tls, auth, 0, 0),
        })
        .into_bytes()
}

pub struct ImapError;
