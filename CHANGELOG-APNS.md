# APNs Fork Changelog

This file documents changes made by this fork on top of upstream
[stalwartlabs/stalwart](https://github.com/stalwartlabs/stalwart) — Apple Push
Notification Service (XAPS) support and everything that comes with it. For
upstream's own changes, see [CHANGELOG.md](CHANGELOG.md).

Fork versions follow `<upstream-version>-apns.<N>`, resetting `N` to 1 each time the
fork rebases onto a newer upstream release. Release candidates leading up to a version
are tagged `-rc.<M>` and marked as a GitHub prerelease.

## [0.16.27-apns.2] - 2026-10-30

### Fixed
- The release builds failed (`v0.16.27-apns.1` has no working artifacts): upstream's `stalwartlabs/opentelemetry-rust` git dependency, pinned by this version's lockfile, is no longer available. It is now taken from `Tyr3al/opentelemetry-rust`, a mirror of the exact same commit (`ae66e97b`), so the binary is unchanged.
- The arm64 FoundationDB image build no longer gets OOM-killed during the final link: it adds the same swapfile the generic ARM builds use.

### Notes
- This is a maintenance release on `release/0.16` for the v0.16.x to v1.0.0 migration path: v1.0.0 can only be reached from a database that is on v0.16.27. No XAPS-specific behavior changed.

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
