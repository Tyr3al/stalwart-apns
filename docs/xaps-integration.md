# XAPS — iOS Mail push notifications (APNs)

This fork adds native support for **XAPS** (`XAPPLEPUSHSERVICE`), the private IMAP extension iOS Mail uses to
register for silent Apple Push Notification service (APNs) pushes on new mail — the same feature
[`dovecot-xaps-plugin`](https://github.com/freswa/dovecot-xaps-plugin) provides for Dovecot, built directly into
Stalwart instead of requiring a separate companion daemon.

## What it does

- When iOS Mail connects, it sends an `XAPPLEPUSHSERVICE` IMAP command with its device token and the mailboxes
  it wants notifications for. Stalwart stores that registration and replies with the APNs topic to use.
- On new mail delivered to **INBOX** (via SMTP, LMTP, or IMAP `APPEND`), Stalwart sends a silent background push
  to every registered device for that account. The phone's existing connection to iCloud/APNs wakes up and the
  Mail app polls the server — this mirrors exactly how Dovecot's plugin/daemon pair behaves, including only
  pushing for `INBOX` (not other mailboxes).
- Non-delivery changes (flags, moves) are batched and sent as a delayed, throttled notification rather than
  immediately, to avoid flooding devices.
- Everything lives in the normal Stalwart process and store — registrations are durable and cluster-safe, so
  any node can register a device and any node can send its pushes; there's no separate daemon or JSON file to
  manage.
- Registered devices can be viewed and managed from the admin panel (**Push → Devices**) or by end users
  themselves (**Account → My Devices**), including a "send test push" button that asks APNs to deliver the
  same silent background notification as a new-message push. A successful test is accepted by APNs and causes
  Mail to refresh; it does not display a visible alert.

`XAPPLEPUSHSERVICE` is undocumented, unofficial Apple/iOS Mail behavior — it isn't a public API, so it could
change or stop working in a future iOS release without notice. It works today.

## Prerequisites

You need a valid **Apple Push Notification service (APNs) credential** before this does anything useful. This
is an external requirement from Apple — Stalwart cannot substitute for it. One of:

- A push certificate obtained via a **macOS Server** purchase, or
- An **Apple Developer Program** membership with the push entitlement, provisioned for the mail topic you'll
  configure (typically `com.apple.mobilemail`, unless you're using a custom topic).

Without this, devices can still register, but no pushes will ever be delivered.

## Enabling the feature

XAPS is an optional Cargo feature (`xaps`), not part of the default build. The published Docker images for this
fork are already built with it — most users don't need to do anything here. If you're building from source
yourself:

```sh
# without XAPS
cargo build --release

# with XAPS
cargo build --release --features xaps
```

## Configuring APNs

In the admin panel, go to **Settings → Push → Apple Push (XAPS)**.

**Settings**
| Field | Description |
|---|---|
| Enabled | Turns XAPS on. Devices won't be able to register, and no pushes will be sent, until this is on *and* a topic + authentication method are configured below. |
| APNs Topic | Must match the topic your APNs credentials were issued for (typically `com.apple.mobilemail`). Returned to iOS Mail during registration. |
| Use Sandbox | Send to Apple's sandbox APNs endpoint instead of production — only relevant if you're using development/sandbox credentials. |
| Delayed Notification Delay (s) | How long to hold non-new-message changes (flags, moves) before sending a batched notification. Default 30s. |
| Delayed Notification Check Interval (s) | How often the delayed-notification queue is checked. Default 20s. |

**APNs Credentials** — configure exactly one authentication method. If more than one is filled in, token auth
wins over PEM, which wins over P12.

| Method | Fields |
|---|---|
| Token (recommended) | Authentication Key (P8), Key ID, Team ID |
| Certificate (PEM) | Client Certificate (PEM), Client Certificate Key (PEM) |
| Certificate (P12) | Client Certificate (P12, base64), Client Certificate P12 Password (leave empty if the P12 isn't password-protected) |

Note on P12: only legacy PBES1 (SHA1+3DES / SHA1+RC2-40) encrypted key bags are supported — the same limitation
the original Go daemon had. P12 files exported with OpenSSL 3.x's PBES2/AES default will be rejected; re-export
with legacy encryption, or use the PEM or token method instead.

Saving with XAPS enabled but no topic or no valid authentication method configured is rejected with a
validation error explaining what's missing.

### One more thing to check

XAPS piggybacks on the same per-node `push_notifications` cluster role WebPush uses, which is **on by default**
— you only need to look at this if you've deliberately disabled it on a node (cluster role settings), in which
case that node won't send XAPS pushes either.

## Permission model

The fork defines three XAPS-specific permissions (`SysXapsGet`, `SysXapsQuery`, `SysXapsUpdate`, IDs 9000–9002)
that guard the **XAPS settings singleton** in the admin panel — the configuration page for APNs credentials and
behavior. `SysXapsGet`/`SysXapsUpdate` also authorize the device-management API
(`/api/xaps/registrations` and `/api/xaps/test`), as an *alternative* to the **generic account-management
permissions**: listing another account's registrations accepts either `SysXapsGet` or `SysAccountGet`, and
deleting registrations or sending a test push accepts either `SysXapsUpdate` or `SysAccountUpdate`. Either
permission is sufficient on its own — device registrations are per-account data, equivalent to other account
state, so the generic permissions naturally apply too. `SysXapsQuery` is not part of this: it stays
settings-only. As before, there is a self-service exception: users always manage their own devices without
requiring any admin permission, and only when accessing another account's devices does one of the admin
permissions above become necessary.

The two permission families default onto different roles (`crates/common/src/auth/permissions.rs`,
`DefaultPermissions`): `sysXaps*` lands on the superuser role only, while `sysAccount*` lands on both the
tenant and superuser roles. In practice this means tenant admins can manage XAPS devices out of the box (via
`sysAccount*`, unchanged from before), while a deployment that wants a narrower, XAPS-only admin role — able to
manage device registrations and push testing but nothing else account-related — can build one from `sysXaps*`
alone.

## Fork numbering: reserved numeric ID ranges

Several registry/`trc` files under `crates/registry/src/schema/` and `crates/trc/src/event/` are marked
`// This file is auto-generated. Do not edit directly.`. This fork has no access to the generator that
produces them, so the XAPS additions in those files are hand-edited. Every numeric discriminant the XAPS
feature adds lives in its own reserved block, kept clear of upstream's own sequential numbering, to avoid
two classes of problems when merging future upstream changes:

1. **Git merge conflicts** on every rebase, if upstream keeps appending its own new variants at the same
   tail these files were edited at.
2. **Silent ID collisions** — worse than a conflict, because these ids are persisted (permission bitmaps
   per role, pickled registry `Property`/`ObjectType` data, `trc` event ids in logs). If upstream ever
   reuses a number this fork already claimed, old stored data can silently resolve to the wrong
   permission/property/event after an upgrade instead of failing to compile.

Since nothing was deployed yet when this was done, the IDs could be moved for free. **Do not renumber
anything below without re-reading this section** — once real data (roles, stored config, logs) references
these numbers, moving them again becomes a breaking change for that data.

### Reserved blocks

| Enum | File(s) | Fork range | Notes |
|---|---|---|---|
| `Permission::SysXaps{Get,Query,Update}` | `crates/registry/src/schema/enums.rs`, `enums_impl.rs` | **9000–9002** | `Permission::COUNT` bumped to `9003`. Kept modest (not pushed to 60000+) because `Permission::COUNT` sizes a real per-role `Bitset` (`crates/common/src/auth/mod.rs`); at 9003 each `Permissions` bitset is ~9 KB (up from ~0.6 KB). Pushing to 65000 would cost ~65 KB per role instead. |
| `ObjectType::Xaps` | `crates/registry/src/schema/properties.rs`, `properties_impl.rs` | **60000** | `ObjectType::COUNT` intentionally left untouched (unused anywhere outside its own impl; still correctly reflects upstream's own object count). |
| `Property::Xaps*` (12 fields) | `crates/registry/src/schema/properties.rs`, `properties_impl.rs` | **60100–60111** | `Property::COUNT` intentionally left untouched, same reasoning as `ObjectType::COUNT`. |
| `trc::EventType::Xaps(XapsEvent::*)` (4 events) | `crates/trc/src/event/enums.rs`, `enums_impl.rs` | **60200–60203** | `TOTAL_EVENT_COUNT` bumped to `60204`. Sizes a single global static array/bitset (`crates/trc/src/ipc/collector.rs`), so the memory cost of a large id (~70 KB, once, globally) is negligible — unlike `Permission::COUNT` this is not per-instance. |
| `trc::MetricType::XapsSuccess` | `crates/trc/src/event/enums.rs`, `enums_impl.rs` | **60300** | Separate ID space from `EventType` (`MetricType` has no `COUNT` constant sizing an array — it's matched by value, not indexed — so no size bump needed). Backs the "Push Notifications Sent" dashboard card; `event_id()` points it at `EventType::Xaps(XapsEvent::Success)` (60200) so it counts the same underlying event. |
| `PrincipalField::XapsRegistrations` | `crates/types/src/field.rs` | **200** | `PrincipalField` is `#[repr(u8)]` (max 255), so there is no 60000-style headroom like the blocks above; the fork instead reserves the high block **200–254**, clear of upstream's own packed range (upstream's `ARCHIVE_FIELD` is `50`, and upstream's sequential principal fields sit at 44–49). The previous id was `46`: it fell inside that packed 44–49 range and was upstream's recycled `EncryptionKeys` principal field id (removed upstream commit `d15efc6f`, Mar 2026) — a silent `rkyv` type-confusion risk (see `crates/store/src/write/serialize.rs`'s `rkyv::access_unchecked`) both on upstream merges and on databases upgraded from pre-2026 Stalwart that may still hold `EncryptionKeys` blobs at that key. Fork deployments that registered devices while the id was 46 lose those registrations on upgrade; devices simply re-register automatically on their next IMAP connect, which is acceptable pre-stable-release. |

`Permission`, `Property`, `ObjectType`, and `trc::EventType` are all `#[repr(u16)]` (max value 65535), so
none of these blocks are close to overflowing, and there's room for another such block if a future
addition needs one — pick an unused thousand-block and add a row above. `PrincipalField` is the
exception: it's `#[repr(u8)]`, so its reserved block is sized in the tens, not thousands (see row above).

### Why the ranges differ in size

`Permission::COUNT` directly sizes a bitset that's instantiated per role / access token
(`PERMISSIONS_BITSET_SIZE` in `crates/common/src/auth/mod.rs`), so pushing it as high as the others would
meaningfully bloat every role's in-memory permission set. `Property::COUNT`, `ObjectType::COUNT` are
declared (required by the `EnumImpl` trait) but never read anywhere else in the codebase, and
`trc::TOTAL_EVENT_COUNT` only sizes one single global static (`Collector.levels` and the interests
bitset), not a per-instance value — so both were free to push far away from upstream's range at
essentially no cost.

### What to check after merging upstream changes

- Re-run `grep -n "NOTE(xaps-fork)"` across the repo and confirm the reserved blocks above are still
  untouched by the merge.
- If upstream ever adds enough new `Permission`/`Property`/`ObjectType`/`trc::EventType` variants to
  approach these reserved blocks (unlikely for a long time given upstream's current pace vs. the headroom
  left), move the fork's block further out and update this table.
- `cargo test -p services -p imap_proto --features xaps` should stay green; it exercises JWT/config
  handling and the IMAP parser, not the numeric registry ids directly, so also spot-check
  `Permission::SysXapsGet.to_id()` etc. still round-trip via `from_id()` if you touch these files again.
