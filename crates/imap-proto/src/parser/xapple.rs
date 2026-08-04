/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-SEL
 */

use crate::{
    Command,
    protocol::xapple,
    receiver::{Request, Token, bad},
};
use compact_str::ToCompactString;

impl Request<Command> {
    pub fn parse_xapple_push_service(self) -> trc::Result<xapple::Arguments> {
        let mut aps_version = None;
        let mut aps_account_id = None;
        let mut aps_device_token = None;
        let mut aps_subtopic = None;
        let mut mailboxes = None;

        let mut tokens = self.tokens.into_iter();

        while let Some(key) = tokens.next() {
            let key = key.unwrap_bytes();
            if key.eq_ignore_ascii_case(b"mailboxes") {
                // The mailboxes value is a parenthesized list: (INBOX Notes)
                match tokens.next() {
                    Some(Token::ParenthesisOpen) => {
                        let mut list = Vec::new();
                        loop {
                            match tokens.next() {
                                Some(Token::ParenthesisClose) | None => break,
                                Some(token) => list.push(
                                    token
                                        .unwrap_string()
                                        .map_err(|v| bad(self.tag.to_compact_string(), v))?,
                                ),
                            }
                        }
                        mailboxes = Some(if list.is_empty() {
                            vec!["INBOX".to_string()]
                        } else {
                            list
                        });
                    }
                    _ => {
                        return Err(bad(
                            self.tag.to_compact_string(),
                            "Invalid arguments.".to_string(),
                        ));
                    }
                }
            } else {
                let value = match tokens.next() {
                    Some(token) => token
                        .unwrap_string()
                        .map_err(|v| bad(self.tag.to_compact_string(), v))?,
                    None => {
                        return Err(bad(
                            self.tag.to_compact_string(),
                            "Invalid arguments.".to_string(),
                        ));
                    }
                };

                if key.eq_ignore_ascii_case(b"aps-version") {
                    aps_version = Some(value);
                } else if key.eq_ignore_ascii_case(b"aps-account-id") {
                    aps_account_id = Some(value);
                } else if key.eq_ignore_ascii_case(b"aps-device-token") {
                    aps_device_token = Some(value);
                } else if key.eq_ignore_ascii_case(b"aps-subtopic") {
                    aps_subtopic = Some(value);
                }
                // Unknown keys are ignored, their value is consumed above.
            }
        }

        // Validate the arguments, mirroring parse_xapplepush() in
        // dovecot-xaps-plugin's xaps-imap-plugin.c.
        if aps_version.as_deref() != Some("2") {
            return Err(bad(
                self.tag.to_compact_string(),
                "Unknown aps-version.".to_string(),
            ));
        }
        if aps_account_id.as_deref().is_none_or(str::is_empty) {
            return Err(bad(
                self.tag.to_compact_string(),
                "Incomplete or empty aps-account-id parameter.".to_string(),
            ));
        }
        if aps_device_token.as_deref().is_none_or(str::is_empty) {
            return Err(bad(
                self.tag.to_compact_string(),
                "Incomplete or empty aps-device-token parameter.".to_string(),
            ));
        }
        if aps_subtopic.as_deref().is_none_or(str::is_empty) {
            return Err(bad(
                self.tag.to_compact_string(),
                "Incomplete or empty aps-subtopic parameter.".to_string(),
            ));
        }
        let mailboxes = mailboxes.ok_or_else(|| {
            bad(
                self.tag.to_compact_string(),
                "Incomplete or empty mailboxes parameter.".to_string(),
            )
        })?;

        Ok(xapple::Arguments {
            tag: self.tag,
            aps_version: aps_version.unwrap_or_default(),
            aps_account_id: aps_account_id.unwrap_or_default(),
            aps_device_token: aps_device_token.unwrap_or_default(),
            aps_subtopic: aps_subtopic.unwrap_or_default(),
            mailboxes,
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::{Command, protocol::xapple, receiver::Receiver};

    fn parse(command: &str) -> Result<xapple::Arguments, ()> {
        let mut receiver = Receiver::new();
        let request = receiver
            .parse(&mut command.as_bytes().iter())
            .map_err(|_| ())?;
        assert_eq!(request.command, Command::XApplePushService);
        request.parse_xapple_push_service().map_err(|_| ())
    }

    #[test]
    fn parse_xapple_push_service() {
        for (command, expected) in [
            (
                "t1 XAPPLEPUSHSERVICE aps-version 2 aps-account-id 0715A26B-CA09-4730-A419-793000CA982E \
                 aps-device-token 2918390218931890821908309283098109381029309829018310983092892829 \
                 aps-subtopic com.apple.mobilemail mailboxes (INBOX Notes)\r\n",
                xapple::Arguments {
                    tag: "t1".into(),
                    aps_version: "2".into(),
                    aps_account_id: "0715A26B-CA09-4730-A419-793000CA982E".into(),
                    aps_device_token: "2918390218931890821908309283098109381029309829018310983092892829".into(),
                    aps_subtopic: "com.apple.mobilemail".into(),
                    mailboxes: vec!["INBOX".into(), "Notes".into()],
                },
            ),
            // Keys are case-insensitive and order-independent.
            (
                "t2 XAPPLEPUSHSERVICE aps-device-token TOKEN aps-version 2 mailboxes (INBOX) \
                 aps-account-id ACCOUNT aps-subtopic com.apple.mobilemail\r\n",
                xapple::Arguments {
                    tag: "t2".into(),
                    aps_version: "2".into(),
                    aps_account_id: "ACCOUNT".into(),
                    aps_device_token: "TOKEN".into(),
                    aps_subtopic: "com.apple.mobilemail".into(),
                    mailboxes: vec!["INBOX".into()],
                },
            ),
            // An empty mailbox list defaults to INBOX.
            (
                "t3 XAPPLEPUSHSERVICE aps-version 2 aps-account-id ACCOUNT aps-device-token TOKEN \
                 aps-subtopic com.apple.mobilemail mailboxes ()\r\n",
                xapple::Arguments {
                    tag: "t3".into(),
                    aps_version: "2".into(),
                    aps_account_id: "ACCOUNT".into(),
                    aps_device_token: "TOKEN".into(),
                    aps_subtopic: "com.apple.mobilemail".into(),
                    mailboxes: vec!["INBOX".into()],
                },
            ),
        ] {
            assert_eq!(
                parse(command).unwrap(),
                expected,
                "Failed to parse {command}"
            );
        }
    }

    #[test]
    fn parse_xapple_push_service_errors() {
        // Missing mailboxes parameter.
        assert!(parse(
            "t1 XAPPLEPUSHSERVICE aps-version 2 aps-account-id A aps-device-token B \
             aps-subtopic com.apple.mobilemail\r\n"
        )
        .is_err());
        // Unknown aps-version.
        assert!(parse(
            "t2 XAPPLEPUSHSERVICE aps-version 1 aps-account-id A aps-device-token B \
             aps-subtopic com.apple.mobilemail mailboxes (INBOX)\r\n"
        )
        .is_err());
        // Empty aps-account-id.
        assert!(parse(
            "t3 XAPPLEPUSHSERVICE aps-version 2 aps-account-id \"\" aps-device-token B \
             aps-subtopic com.apple.mobilemail mailboxes (INBOX)\r\n"
        )
        .is_err());
        // Missing value.
        assert!(parse("t4 XAPPLEPUSHSERVICE aps-version\r\n").is_err());
        // Mailboxes without a list.
        assert!(parse(
            "t5 XAPPLEPUSHSERVICE aps-version 2 aps-account-id A aps-device-token B \
             aps-subtopic com.apple.mobilemail mailboxes INBOX\r\n"
        )
        .is_err());
    }

    #[test]
    fn parse_long_command_name() {
        // XAPPLEPUSHSERVICE is 17 characters, longer than the previous 15
        // character command name limit.
        let mut receiver: Receiver<Command> = Receiver::new();
        let request = receiver
            .parse(
                &mut "t1 XAPPLEPUSHSERVICE aps-version 2 aps-account-id A aps-device-token B \
                       aps-subtopic com.apple.mobilemail mailboxes (INBOX)\r\n"
                    .as_bytes()
                    .iter(),
            )
            .unwrap();
        assert_eq!(request.command, Command::XApplePushService);

        // Commands longer than 32 characters are still rejected.
        let mut receiver: Receiver<Command> = Receiver::new();
        assert!(receiver
            .parse(
                &mut "t2 XAPPLEPUSHSERVICEEXTRA aps-version 2\r\n"
                    .as_bytes()
                    .iter(),
            )
            .is_err());
    }
}
