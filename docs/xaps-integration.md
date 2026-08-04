# Integrating dovecot-xaps (iOS push email) into Stalwart

Status: plan approved — Phases 1–3 ✅ complete (IMAP `XAPPLEPUSHSERVICE` extension, registration store, config section, APNs sender, push-manager notify hook, delayed-notification throttling, PEM cert auth). Remaining: P12 cert auth, dedicated trc event types, live multi-node verification.

## Background: what the original system does

**`dovecot-xaps-plugin`** (C, two dovecot plugins):
1. **`xaps-imap-plugin.c`** — adds the `XAPPLEPUSHSERVICE` IMAP capability (pre-LOGIN, in the greeting) and a
   command of the same name. iOS Mail sends:
   ```
   XAPPLEPUSHSERVICE aps-version 2 aps-account-id <uuid> aps-device-token <token>
                     aps-subtopic com.apple.mobilemail mailboxes (INBOX Notes)
   ```
   The plugin POSTs `{"ApsAccountId","ApsDeviceToken","ApsSubtopic","Username","Mailboxes"}` as JSON to the
   daemon's `/register`, gets back the **aps-topic** (certificate subject UID), and replies
   `* XAPPLEPUSHSERVICE aps-version 2 aps-topic <topic>` + `OK XAPPLEPUSHSERVICE completed.`
2. **`xaps-push-notification-plugin.c`** — a dovecot *push-notification driver* fired on delivery (LDA/LMTP).
   On every message event it POSTs `{"Username","Mailbox","Events":["MessageNew",…]}` to the daemon's `/notify`.

**`dovecot-xaps-daemon`** (Go, ~700 LOC total):
- `internal/socket.go` — HTTP server, two routes: `/register` (validates subtopic == `com.apple.mobilemail`,
  stores the registration, returns the topic) and `/notify` (lowercases username, **ignores everything except
  `INBOX`**, looks up registrations containing `INBOX`, sends one APNs push per device, deletes the registration
  on APNs `410`).
- `internal/apns.go` — APNs client (`sideshow/apns2`), payload always `{"aps":{"account-id":"<accountId>"}}`,
  `PushType=background`; auth via P12 cert, PEM cert, or token (P8 key + keyId + teamId); topic from cert subject
  or config; delay-map throttles non-`MessageNew` events (30 s delay, 20 s check).
- `internal/database/database.go` — JSON file keyed `username → account_id → {device_token, mailboxes,
  registration_time}`; atomic write, 15-min flush, 30-day stale-registration cleanup.

The point: the phone's *existing* iCloud connection gets a silent push; it then polls the server.

## Target architecture in Stalwart (best case: both components in-process)

```
iOS Mail ──IMAP──► stalwart imap service
                    │  XAPPLEPUSHSERVICE cmd
                    ▼
              registrations ──store (durable, cluster-safe)──► push manager
                                                                   │ EmailPush event
SMTP/LMTP/IMAP APPEND ──► email_ingest ──► broadcast_push_notification
                                                                   ▼
                                                       APNs sender (reqwest/http2)
                                                                   │
                                                              Apple APNs
```

Everything in one process — **no HTTP endpoints `/register` / `/notify`**. The IMAP handler writes registrations
to the store directly; the push manager sends APNs directly. The daemon's single-node JSON file is replaced by
per-account durable store data (same pattern as JMAP WebPush subscriptions), which also fixes the daemon's
multi-node weakness.

| Concern | Original component | Stalwart equivalent |
|---|---|---|
| IMAP extension + registration | `xaps-imap-plugin.c` | new op handler in `crates/imap` + `imap-proto` |
| Registration persistence | `database.go` (JSON file) | per-account store property (`PrincipalField::XapsRegistrations`) |
| New-mail → push | `xaps-push-notification-plugin.c` + `/notify` | branch in push manager's `Event::Push` handler |
| APNs transport | `apns.go` + apns2 lib | new `apns` module, reqwest + rustls + ES256 JWT |

## Integration point A — IMAP `XAPPLEPUSHSERVICE` (the "plugin")

All confirmed against the current code:

1. **Command enum**: add `XApplePushService` to `Command` — `crates/imap-proto/src/lib.rs:16-81`.
2. **Name→variant map**: `"XAPPLEPUSHSERVICE" => Command::XApplePushService` in the `tiny_map!` —
   `crates/imap-proto/src/parser/mod.rs:40-85`.
3. **⚠️ Command length cap**: stalwart caps command names at **15 chars**
   (`crates/imap-proto/src/receiver.rs:181-186`); `XAPPLEPUSHSERVICE` is **17**. Bump the `push_checked(ch, 15)`
   limit to a const (32). Nothing else assumes 15; RFC 3501 imposes no such limit.
4. **Argument parsing**: the tokenizer yields `Vec<Token>` where `(`/`)` are `ParenthesisOpen/Close`
   (`receiver.rs:218-311`), so the command's key/value pairs + `mailboxes (...)` list arrive as a generic token
   stream. New `parser/xapple.rs` parses them (mirrors `parse_xapplepush()` in `xaps-imap-plugin.c`).
5. **Dispatch**: arm in `ingest()` — `crates/imap/src/core/client.rs:96-255` — calling a new
   `handle_xapple_push_service()` in a new `crates/imap/src/op/xapple.rs` (registered in `op/mod.rs`).
6. **Auth gating**: add `Command::XApplePushService` to the authenticated bucket in `is_allowed()` —
   `crates/imap/src/core/client.rs:364-395` (mirror `Command::GetJmapAccess` at `:386`).
7. **Capability**: `Capability::XApplePushService` in the enum + `serialize()` + the **base (pre-auth)** list of
   `all_capabilities()` (`crates/imap-proto/src/protocol/capability.rs:15-183`) — Apple advertises it in the
   greeting; stalwart re-advertises post-LOGIN from the same list.
8. **Reply**: `* XAPPLEPUSHSERVICE aps-version 2 aps-topic <topic>` + `OK XAPPLEPUSHSERVICE completed.`
   (add the `Display` arm in `protocol/mod.rs:716-770`).

Handler behavior (in-process, async, no HTTP round trip):
- Load aps-topic (Phase 1: placeholder const; **Phase 2: config section derived from APNs credentials**).
- Upsert registration `{aps_account_id, device_token, mailboxes, registered_at}` into the store
  (per-account, keyed by `account_id` — better than the daemon's lowercased-username key; handles aliases).
- Reply as above. This replaces both `/register` and the daemon's `AddRegistration`.

## Integration point B — new-mail → push (the "daemon" half)

- Every successful delivery fires `broadcast_push_notification(PushNotification::EmailPush(EmailPush{account_id,
  email_id, change_id}))` right after the store commit — `crates/email/src/message/delivery.rs:202-212`
  (covers SMTP/LMTP *and* IMAP APPEND through the same `email_ingest`).
- It flows: `push_tx` → `spawn_push_router` (`crates/services/src/state_manager/manager.rs:25-189`) → push
  manager `Event::Push` handler (`crates/services/src/state_manager/push.rs:312-393`).

**Add a XAPS branch in the push manager's `Event::Push` handler**:
1. Load XAPS registrations for `account_id` from the store.
2. Resolve which mailbox the message landed in — `MessageData.mailboxes` (same read `build_email_push_object`
   does, `crates/services/src/state_manager/email_push.rs:60-100`). Reproduce the original semantics exactly:
   **push only if the message landed in `INBOX`** and the registration's mailbox list contains `INBOX`
   (daemon `socket.go` `handleNotify`).
3. For each matching registration, send APNs push `{"aps":{"account-id":"<aps_account_id>"}}` (async; failures
   logged, never block delivery).
4. On APNs `410` → delete the registration (daemon `apns.go`).

**New module `crates/services/src/state_manager/apns.rs`**:
- Transport: `reqwest` with `http2` (already a dep of `crates/services`, `Cargo.toml:36`; helper
  `build_http_client` in `crates/utils/src/http.rs:14-35`). APNs mandates HTTP/2 (ALPN negotiates h2).
- Auth mode 1 (**recommended, token**): ES256 JWT `{iss: teamId, iat}` with `kid` header signed with the P8 key —
  stalwart already does ES256 JWT for WebPush VAPID (`crates/common/src/network/webpush.rs`); cache JWT, refresh
  hourly (APNs rejects tokens > 1 h).
- Auth mode 2 (cert): PEM client cert via rustls identity; P12 optional.
- Endpoint `https://api.push.apple.com/3/device/<token>` (sandbox variant as config), headers `apns-topic`,
  `apns-push-type: background`, `apns-priority: 5`, `apns-expiration`. Non-200: `410` → drop registration;
  others → retry with backoff + log.

## Integration point C — config & wiring

- New config section (e.g. `xaps`) in the registry schema — `crates/registry/src/schema/{structs,enums,
  properties}*.rs`. These files are marked "auto-generated, do not edit directly" — **confirm with the
  maintainers how they are regenerated** before hand-editing (or hand-edit following the exact patterns of
  existing singletons, as a fork).
- Keys: `enabled`, APNs auth (prefer `keyFileP8` + `keyFileKeyId` + `keyFileTeamId` + `keyFileTopic`, plus
  `certificateFilePem`/`P12` for cert auth), `sandbox` (dev APNs), `delay`/`checkInterval` (if Phase 3).
- The push manager already runs only when role `push_notifications` is on
  (`crates/services/src/state_manager/push.rs:45`, role def `crates/common/src/config/network.rs:84`); XAPS
  piggybacks on it. No new listener/service needed.

## Phased implementation

**Phase 1 — IMAP extension + registration store** ✅ done
1. Bump command-name cap (15 → 32); add `Command` variant, map entry, `Display` arm.
2. Add capability (enum, wire string, base pre-auth list).
3. Token parser `parser/xapple.rs` + `op/xapple.rs` handler + dispatch arm + auth gating.
4. Registration persistence as per-account store property (`PrincipalField::XapsRegistrations`), incl. 30-day
   staleness pruning (port `database.go` `cleanupRegistered`).
5. Unit tests: parser (valid/invalid), capability presence, receiver command-name length.

**Phase 2 — APNs sender + notify hook** ✅ done
1. `apns.rs` transport with token auth (P8), JWT caching, `410` handling
   (`SendResult::DeviceTokenInactive` → registration deleted).
2. XAPS branch in the push manager (`push.rs`): accounts with device registrations are registered with the push
   router via `PushServerRegister` (startup load + `Event::Update` on `PushServerUpdate` broadcast); on
   `Event::Push` with an `EmailPush` for a registered account, `deliver_xaps_notifications` runs (INBOX-only,
   reads `MessageData.mailboxes`, checks `INBOX_ID = 0`).
3. Config: new `xaps` registry singleton (`ObjectType::Xaps`, `SysXaps{Get,Query,Update}` permissions,
   `resources/schema/schema.json` admin-UI entry), runtime `XapsConfig` in `Core`, validation when `enabled`
   without credentials. Replaces the Phase-1 placeholder aps-topic.
4. Tests: JWT format/caching + sandbox host selection (`apns.rs`), plus the Phase-1 parser/upsert/prune tests.
   Note: an **Apple push certificate is mandatory** (macOS Server purchase or paid Developer account) — a hard
   external prerequisite of the whole XAPS idea.

Design notes (Phase 2):
- Requires the `push_notifications` role (same as WebPush) and `xaps.enabled`.
- The router's per-account `is_push` flag is shared between WebPush and XAPS; unregistration is mutually
  exclusive (an account is only unregistered when it has neither WebPush subscriptions nor XAPS devices) — see
  `push.rs` `Event::Update`.
- Registration changes propagate cluster-wide via `PushServerUpdate { broadcast: true }`; delivery is
  shard-consistent (`jmap.push_total_shards` / `cluster_push_shard`), so exactly one node sends each push.

**Phase 3 — fidelity & polish** ✅ done
1. Delayed-notification throttling ported (the daemon's `delayedApns` map): non-delivery mailbox changes
   (`StateChange` with `Email`/`Mailbox`/`Thread` types, excluding deliveries) schedule a batched push per
   device, sent `delay` seconds later and checked every `checkInterval` — see `spawn_xaps_delayed` in
   `apns.rs` and the `Event::Push` branch in `push.rs`. New-message pushes cancel pending entries for the
   devices they reach (daemon parity).
2. PEM client-certificate auth (`certificateFilePem` + `certificateFilePemKey`) as an alternative to token
   auth; sandbox vs production endpoint already configurable. Config validation requires exactly one auth
   method when enabled.
3. `delay`/`checkInterval` config (defaults 30s/20s, same as `xapsd.yaml`) + admin-UI schema fields.

Remaining (out of scope / external):
- P12 cert auth (would need an additional PFX-parsing dependency; PEM and token auth cover both modern and
  legacy certificate flows).
- Dedicated `trc` event types for XAPS (currently reuses `PushSubscriptionEvent`).
- Live multi-node verification and a mock-APNs end-to-end test harness (needs an Apple push certificate).

## Alternatives & risks

- **Fallback/hybrid**: Phase-1's IMAP command plus an HTTP POST to the existing Go daemon (`/register`,
  `/notify`) is a small subset of this plan, but keeps two processes and loses multi-node/durability. Not
  recommended as the end state.
- **15-char command cap bump**: contained change; must not break the 128-char tag path or existing tests.
- **Registry pickle backward-compat**: registry config objects are stored pickled
  (`Object::deserialize_with_key`, `crates/store/src/registry/mod.rs:87`); appending fields to *existing* structs
  breaks unpickling of old data (silent reset to defaults). New singleton sections are safe (absent → default).
  This is why the config section is a new `xaps` singleton, not extra keys on `Imap`.
- **Apple prerequisites & legal**: the extension is undocumented Apple behavior; a push certificate from an
  Apple ID that owns macOS Server (or a Developer account with the push entitlement) is required. Write a fresh
  Rust implementation from the observed wire protocol — don't copy the C/Go code verbatim (Apple's original
  dovecot patches are APSL; the MIT reimplementations can inform behavior but should be re-derived).
- **Behavioral fidelity**: original only pushes for `INBOX` deliveries to devices that registered `INBOX`;
  replicate exactly to avoid surprising users.
- **Cluster behavior**: registrations in the store mean any node can serve the IMAP command and any node can
  send pushes — strictly better than the original.
