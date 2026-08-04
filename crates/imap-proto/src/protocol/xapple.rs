/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-SEL
 */

//! Arguments for the XAPPLEPUSHSERVICE command, the undocumented IMAP extension
//! used by iOS Mail to register for native push notifications.
//!
//! ```text
//! XAPPLEPUSHSERVICE aps-version 2 aps-account-id <uuid> aps-device-token <token>
//!                   aps-subtopic com.apple.mobilemail mailboxes (INBOX Notes)
//! ```

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Arguments {
    pub tag: String,
    pub aps_version: String,
    pub aps_account_id: String,
    pub aps_device_token: String,
    pub aps_subtopic: String,
    pub mailboxes: Vec<String>,
}

#[cfg(test)]
mod tests {
    use crate::{Command, StatusResponse};

    #[test]
    fn serialize_reply() {
        // The wire format iOS Mail validates against, mirroring the reply of
        // dovecot-xaps-plugin's xaps-imap-plugin.c.
        let response = StatusResponse::completed(Command::XApplePushService)
            .with_tag("t1")
            .serialize(
                b"* XAPPLEPUSHSERVICE aps-version 2 aps-topic com.apple.mail\r\n".to_vec(),
            );
        assert_eq!(
            response,
            b"* XAPPLEPUSHSERVICE aps-version 2 aps-topic com.apple.mail\r\n\
              t1 OK XAPPLEPUSHSERVICE completed\r\n"
        );
    }
}
