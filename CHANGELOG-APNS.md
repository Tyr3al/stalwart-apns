# APNs Fork Changelog

This file documents changes made by this fork on top of upstream
[stalwartlabs/stalwart](https://github.com/stalwartlabs/stalwart) — Apple Push
Notification Service (XAPS) support and everything that comes with it. For
upstream's own changes, see [CHANGELOG.md](CHANGELOG.md).

Fork versions follow `<upstream-version>-apns.<N>`, resetting `N` to 1 each time the
fork rebases onto a newer upstream release. Release candidates leading up to a version
are tagged `-rc.<M>` and marked as a GitHub prerelease.

## [Unreleased]

## [1.1.1-apns.1] - 2026-11-16

### Changed
- Rebased onto upstream [v1.1.1](https://github.com/stalwartlabs/stalwart/blob/main/CHANGELOG.md) (from v1.0.1), picking up upstream v1.0.2, v1.0.3, v1.1.0 and v1.1.1. No XAPS-specific behavior changed. Upstream's passkeys, Stalwart Keep store, encryption-at-rest objects and permissions, and expanded admin schema are picked up as-is; the XAPS settings, permissions, menu entries and dashboard cards were re-applied to the regenerated v1.1.1 schema. The XAPS Overview cards now use upstream's `items`/`stat` dashboard layout.
- CI and the Docker images now build both the fork's `xaps` feature and upstream's new `keep` feature.
- `KV_RATE_LIMIT_XAPS` moved from id 20 to 21: upstream v1.1.1 claimed 20 for `KV_KEY_LOOKUP`. Only transient XAPS test-push rate-limit state uses this key, so no persisted data is affected.
- WebUI: works with `Tyr3al/webui-apns` `2.0.6-apns.1` and later 2.x.

### Upgrading
- Upstream v1.1.0 introduced `sysPasskey*` permissions for the default roles. After upgrading, either add them manually with the recovery administrator or restart once with `STALWART_UPDATE_ROLE_PERMISSIONS=true`, then remove the variable. See upstream's [default roles on upgraded servers](https://stalw.art/docs/auth/authorization/permissions#default-roles-on-upgraded-servers) note.

## [1.0.1-apns.2] - 2026-10-31

### Fixed
- The WebUI never loaded: `Semver::current()` parsed the full crate version `1.0.1-apns.1` and panicked on the `-apns.1` suffix whenever an application bundle declared `minServerVersion` (as `webui-apns` 2.x does), logging "Bundle unpack task panicked" and serving no bundle for `admin` and `account`. The pre-release and build-metadata suffix is now ignored, like `parse_version` already does for bundle versions.

## [1.0.1-apns.1] - 2026-10-30

### Changed
- Rebased onto upstream [v1.0.1](https://github.com/stalwartlabs/stalwart/blob/main/CHANGELOG.md) (from v1.0.0). No XAPS-specific behavior changed. Upstream's new OpenTelemetry/Prometheus exporters and the `telemetry` crate are picked up as-is; the XAPS counters and the "Registered Devices" gauge keep working through the existing metric plumbing.
- The admin schema was regenerated from upstream's 1.0.1 schema with the XAPS settings, permissions, menu entries and dashboard cards re-applied.
- WebUI: works with `Tyr3al/webui-apns` `2.0.1-apns.1` and later 2.x (the matching rebase onto upstream WebUI 2.0.2/2.0.3 ships as `2.0.3-apns.1`).

## [1.0.0-apns.1] - 2026-10-27

### Changed
- Rebased onto upstream [v1.0.0](https://github.com/stalwartlabs/stalwart/blob/main/CHANGELOG.md). This is a major upstream release with a new database layout: existing v0.16.x deployments (including `0.16.x-apns.N`) cannot be upgraded in place. Follow upstream's [migration documentation](https://stalw.art/docs/migration/upgrade-0-16/). XAPS device registrations are not carried over; devices re-register on their next IMAP connect.
- The WebUI bundle must be a v2 bundle: upstream now rejects any other major version. The fork's default stays `Tyr3al/webui-apns/releases/latest`, which is expected to serve the fork's WebUI v2 build; an existing `Application` that still points at a v1 bundle has to be switched to it.
- The XAPS settings field `enabled` is now `enable`, matching upstream's naming for every other object.
- Upstream removed the in-tree integration test suite (`tests/`); the fork's CI and `Dockerfile.fdb` no longer reference it.
- `XAPPLEPUSHSERVICE` is no longer part of the cached IMAP greeting/capability lists; it is appended per connection only while XAPS is enabled and fully configured.
- Ported to the new IMAP parser (command names longer than 15 characters are still accepted, now up to 32), the compact `MessageData` format (the INBOX check for new-mail pushes now reads it through `message_datas`), and the new registry object layout (`Xaps` is boxed in `ObjectInner`).
- `p256` moved to 0.14 along with upstream; the APNs JWT and PKCS#12 handling is unchanged.

## [0.16.27-apns.1] - 2026-10-19

### Changed
- Rebased onto upstream [v0.16.27](https://github.com/stalwartlabs/stalwart/blob/main/CHANGELOG.md). No XAPS-specific behavior changed.
- Upstream now pins its default WebUI to `stalwartlabs/webui` v1.0.11 and rejects bundles with a major version above 1. The fork's default stays `Tyr3al/webui-apns/releases/latest`, so WebUI fixes still arrive via the Application's update; the new major-version guard applies to it as well.
- Release tags no longer publish the generic `ghcr.io/tyr3al/stalwart-apns` images; the FoundationDB image `ghcr.io/tyr3al/stalwart-apns/stalwart-apns:<version>` is now built and published by CI for every release (amd64 + arm64).

## [0.16.26-apns.1] - 2026-10-17

### Changed
- Rebased onto upstream [v0.16.26](https://github.com/stalwartlabs/stalwart/blob/main/CHANGELOG.md) (from v0.16.16). No XAPS-specific behavior changed; the `XAPPLEPUSHSERVICE` capability is still only advertised once XAPS is enabled and fully configured, now alongside upstream's `LOGINDISABLED` handling for clear-text connections.

### Fixed
- The "Registered Devices" live gauge on the admin Overview dashboard always read 0, even with devices registered. The metric was computed correctly and updated on its `AtomicGauge`, but was never included in the list `collect_gauges()` streams to clients (live SSE feed, Prometheus, OTel export), so the value never reached any consumer.

## [0.16.16-apns.1] - 2026-08-13

First stable release of the fork.

### Added
- Apple Push Notification Service (XAPS) support for instant IMAP push to iOS/macOS Mail, compatible with the [dovecot-xaps-daemon](https://github.com/freswa/dovecot-xaps-daemon) protocol:
  - Configurable push settings (APNs topic, team ID, signing key, sandbox mode, delivery delay), with P12 (PKCS#12) and PEM certificate auth.
  - `sysXaps*` permissions, synced onto existing roles automatically, authorizing both the settings object and the device-management API — so a narrower XAPS-only admin role can be delegated.
  - Admin "Push Devices" page listing every account's registered devices, with per-device delete, per-account remove-all, and a "Send test push" action.
  - Self-service "My Devices" page, under the Account menu, for users to manage their own registered devices.
  - "Push Notifications Sent" counter card on the Overview and Delivery dashboards, and a live "Registered Devices" counter card on the Overview dashboard.
  - Push delivery attempts and device registrations are logged at Info level, including APNs' rejection reason on failure.

### Changed
- Bundled WebUI now points at [Tyr3al/webui-apns](https://github.com/Tyr3al/webui-apns), the fork's own frontend with the matching XAPS management UI, instead of upstream `stalwartlabs/webui`.
