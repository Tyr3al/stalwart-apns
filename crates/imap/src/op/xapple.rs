/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-SEL
 */

use crate::core::Session;
use common::{
    ipc::PushEvent,
    network::SessionStream,
};
use email::push::xaps::{
    XAPS_MAX_REGISTRATIONS, XapsRegistration, XapsRegistrations,
};
use imap_proto::{
    Command, ResponseType, StatusResponse,
    receiver::Request,
};
use registry::schema::enums::Permission;
use store::{
    Serialize, ValueKey,
    write::{AlignedBytes, Archive, Archiver, BatchBuilder, now},
};
use trc::{AddContext, ServerEvent, XapsEvent};
use types::{collection::Collection, field::PrincipalField};

// Default APNs topic used when none is configured, mirroring the default
// "keyFileTopic" of dovecot-xaps-daemon's xapsd.yaml.
const APS_TOPIC: &str = "com.apple.mail";

impl<T: SessionStream> Session<T> {
    pub async fn handle_xapple_push_service(
        &mut self,
        request: Request<Command>,
    ) -> trc::Result<()> {
        // Validate access
        self.assert_has_permission(Permission::ImapCapability)?;

        let arguments = request.parse_xapple_push_service()?;

        // The extension is only functional when XAPS is enabled and fully
        // configured.
        if !self.xaps_ready() {
            return Err(trc::ImapEvent::Error
                .into_err()
                .details("XAPPLEPUSHSERVICE is not enabled.")
                .ctx(trc::Key::Type, ResponseType::Bad)
                .id(arguments.tag));
        }

        // Only the subtopic used by iOS Mail is supported.
        if arguments.aps_subtopic != "com.apple.mobilemail" {
            return Err(trc::ImapEvent::Error
                .into_err()
                .details("Unknown aps-subtopic.")
                .ctx(trc::Key::Type, ResponseType::Bad)
                .id(arguments.tag));
        }

        let data = self.state.session_data();
        let account_id = data.account_id;

        // Load existing registrations, pruning stale ones.
        let current_time = now();
        let registrations_archive = self
            .server
            .store()
            .get_value::<Archive<AlignedBytes>>(ValueKey::property(
                account_id,
                Collection::Principal,
                0,
                PrincipalField::XapsRegistrations,
            ))
            .await
            .caused_by(trc::location!())?;
        let mut registrations = if let Some(registrations) = &registrations_archive {
            registrations
                .deserialize::<XapsRegistrations>()
                .caused_by(trc::location!())?
        } else {
            XapsRegistrations::default()
        };
        registrations.prune(current_time);

        // Reject new devices once the per-account limit is reached.
        let is_new_device = !registrations
            .registrations
            .iter()
            .any(|r| r.aps_account_id == arguments.aps_account_id);
        if is_new_device && registrations.registrations.len() >= XAPS_MAX_REGISTRATIONS {
            return Err(trc::ImapEvent::Error
                .into_err()
                .details("Too many devices registered.")
                .ctx(trc::Key::Type, ResponseType::Bad)
                .id(arguments.tag));
        }

        // Upsert this device's registration.
        let device_id = arguments.aps_account_id.clone();
        let changed = registrations.upsert(XapsRegistration {
            aps_account_id: arguments.aps_account_id,
            device_token: arguments.aps_device_token,
            mailboxes: arguments.mailboxes,
            registered_at: current_time,
        });

        // Persist the registrations.
        let mut batch = BatchBuilder::new();
        if registrations_archive.is_none() {
            batch
                .with_account_id(u32::MAX)
                .with_collection(Collection::Principal)
                .with_document(account_id)
                .tag(PrincipalField::XapsRegistrations);
        }
        batch
            .with_account_id(account_id)
            .with_collection(Collection::Principal)
            .with_document(0);
        if let Some(registrations_archive) = registrations_archive {
            batch.assert_value(PrincipalField::XapsRegistrations, registrations_archive);
        }
        batch.set(
            PrincipalField::XapsRegistrations,
            Archiver::new(registrations)
                .serialize()
                .caused_by(trc::location!())?,
        );
        self.server
            .commit_batch(batch)
            .await
            .caused_by(trc::location!())?;

        if changed {
            trc::event!(
                Xaps(XapsEvent::Registered),
                Details = if is_new_device {
                    format!("XAPS device {device_id} registered for account {account_id}")
                } else {
                    format!("XAPS device {device_id} re-registered for account {account_id}")
                },
            );
        }

        // Notify the push manager so it forwards new-mail events for this
        // account to the XAPS push path.
        if self
            .server
            .inner
            .ipc
            .push_tx
            .clone()
            .send(PushEvent::PushServerUpdate {
                account_id,
                broadcast: true,
            })
            .await
            .is_err()
        {
            trc::event!(
                Server(ServerEvent::ThreadError),
                Details = "Error sending push updates.",
                CausedBy = trc::location!()
            );
        }

        // Reply with the aps-topic, which the device uses to validate pushes.
        let response = format!(
            "* XAPPLEPUSHSERVICE aps-version {} aps-topic {}\r\n",
            arguments.aps_version,
            self.server.core.xaps.topic.as_deref().unwrap_or(APS_TOPIC)
        )
        .into_bytes();
        self.write_bytes(
            StatusResponse::completed(Command::XApplePushService)
                .with_tag(arguments.tag)
                .serialize(response),
        )
        .await
    }
}
