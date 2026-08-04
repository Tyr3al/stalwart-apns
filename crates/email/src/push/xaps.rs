/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-SEL
 */

//! XAPS (Apple Push Service) device registrations, the in-process equivalent of
//! the device database kept by the dovecot-xaps-daemon. Registrations are
//! stored per account in the Principal collection.

/// Registrations not refreshed within this period are pruned, mirroring the
/// 30 day cleanup of dovecot-xaps-daemon's database.go.
pub const XAPS_REGISTRATION_TTL_SECS: u64 = 30 * 24 * 60 * 60;

/// Maximum number of devices that can register for a single account, to bound
/// the size of the per-account registrations property.
pub const XAPS_MAX_REGISTRATIONS: usize = 8;

#[derive(
    rkyv::Archive, rkyv::Deserialize, rkyv::Serialize, Default, Debug, Clone, PartialEq, Eq,
)]
pub struct XapsRegistration {
    /// Unique id the iOS device has associated with this account.
    pub aps_account_id: String,
    /// The APS device token.
    pub device_token: String,
    /// Mailboxes to send notifications for.
    pub mailboxes: Vec<String>,
    /// Unix timestamp of the last registration.
    pub registered_at: u64,
}

#[derive(
    rkyv::Archive, rkyv::Deserialize, rkyv::Serialize, Default, Debug, Clone, PartialEq, Eq,
)]
pub struct XapsRegistrations {
    pub registrations: Vec<XapsRegistration>,
}

impl XapsRegistrations {
    /// Adds or updates a registration, identified by `aps_account_id`.
    /// Returns true if the set of registrations changed.
    pub fn upsert(&mut self, registration: XapsRegistration) -> bool {
        for existing in self.registrations.iter_mut() {
            if existing.aps_account_id == registration.aps_account_id {
                if *existing != registration {
                    *existing = registration;
                    return true;
                }
                return false;
            }
        }
        self.registrations.push(registration);
        true
    }

    /// Removes registrations that were not refreshed within
    /// `XAPS_REGISTRATION_TTL_SECS`. Returns true if any were removed.
    pub fn prune(&mut self, current_time: u64) -> bool {
        let len = self.registrations.len();
        self.registrations
            .retain(|r| r.registered_at + XAPS_REGISTRATION_TTL_SECS >= current_time);
        len != self.registrations.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registration(id: &str, registered_at: u64) -> XapsRegistration {
        XapsRegistration {
            aps_account_id: id.to_string(),
            device_token: "token".to_string(),
            mailboxes: vec!["INBOX".to_string()],
            registered_at,
        }
    }

    #[test]
    fn upsert() {
        let mut registrations = XapsRegistrations::default();

        // Insert.
        assert!(registrations.upsert(registration("a", 1)));
        assert_eq!(registrations.registrations.len(), 1);

        // Idempotent update returns false.
        assert!(!registrations.upsert(registration("a", 1)));
        assert_eq!(registrations.registrations.len(), 1);

        // Updated registration replaces the existing one.
        assert!(registrations.upsert(registration("a", 2)));
        assert_eq!(registrations.registrations.len(), 1);
        assert_eq!(registrations.registrations[0].registered_at, 2);

        // Distinct accounts accumulate.
        assert!(registrations.upsert(registration("b", 1)));
        assert_eq!(registrations.registrations.len(), 2);
    }

    #[test]
    fn prune() {
        let mut registrations = XapsRegistrations {
            registrations: vec![
                registration("fresh", 1_000_000),
                registration("stale", 1),
                registration("oldest", 0),
            ],
        };

        // TTL is 30 days; everything older than that is removed.
        assert!(registrations.prune(1_000_000 + XAPS_REGISTRATION_TTL_SECS));
        assert_eq!(registrations.registrations.len(), 1);
        assert_eq!(registrations.registrations[0].aps_account_id, "fresh");

        // Nothing left to prune.
        assert!(!registrations.prune(1_000_000 + XAPS_REGISTRATION_TTL_SECS));
    }
}
