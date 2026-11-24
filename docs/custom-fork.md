# Custom fork

`BierDav/palpo` builds the `patched` branch for `quickme.at`. The fork follows
upstream `main`, rather than the latest stable tag. Its patches are listed in
`.fork-kit.toml`. Custom image tags use the newest upstream release tag in the
merged history (`<version>-custom.<n>`), which `custom-release.yml` reads from
git.

## Keeping up with upstream

Since 2026-11-24, [fork-kit](https://github.com/BierDav/fork-kit)
(`.github/workflows/fork-kit.yml`) replaces the scheduled sync script. Every
six hours it merges upstream `main` into `fork-kit/sync` and keeps one pull
request into `patched` open for it.

- **Clean merge, required checks green, no upstream `.github/` changes:** the
  PR merges itself. The push to `patched` releases, and Renovate deploys the
  tested digest as before. The required checks are:
  - `Rustfmt and Clippy`;
  - `Cargo check stable - x86_64-unknown-linux-gnu`, which includes the fork's
    database regressions;
  - `Complement`;
  - `Build & test (amd64)` and `Build & test (arm64)`, which run
    `tests/custom_idle_sync.py`.
- **Conflicts:** the PR is labelled `fork-kit:conflict` and names the patches
  involved. Resolve them on `fork-kit/sync` as described under "Future
  conflicts" in the platform's `docs/palpo-upstream-sync.md`, then merge the PR.
- **Upstream changed `.github/`:** nothing is pushed until someone has reviewed
  the change and run the fork-kit workflow with `allow-workflow-changes`.
- **Keeping a PR from merging itself:** add the label `fork-kit:hold`.

`quality-control.yml` and `complement.yml` run only when a PR touches Rust,
Cargo or test files. An upstream change touching none of those leaves the sync
PR waiting until the next upstream change that does.

On 2026-11-16 the fork moved to upstream
`a7b0f48bf8d2ec140dd408266b9657df9f13525c`.

- Removed the custom fix for [#649](https://github.com/palpo-im/palpo/issues/649).
  Upstream [#665](https://github.com/palpo-im/palpo/pull/665) now supplies the
  idle key counts and storage-error handling, with its own database regression.
- Dropped the old custom E2EE state-diff optimization because upstream replaced
  that implementation with publication-based device tracking.
- Retained incremental active-room filtering, prompt local sync wakeups,
  independent to-device acknowledgements, and invite-to-join state delivery.
- Fixed room tags at `/user/{user_id}/rooms/{room_id}/tags`, with flat tag
  information in PUT bodies and authorization of the path user.
- Fixed read-count updates to use the receipt's stream position, rather than
  the position of the old message it reads. Sliding sync delivers the new
  counts even when receipts are disabled and there are no timeline changes.

Upstream is merged into the complete fork history without rewriting it.
Custom release notes start at the merged upstream revision, so upstream history
is not listed as custom patches.

The isolated HTTP regressions for room preferences and invite delivery are
registered in default/all-feature database CI. The custom Docker fixture also
checks standard tag bodies, account-data echoes and cleared unread counters on
both release architectures.


On 2026-11-17 the fork merged upstream `3e036e82` after scheduled patch replay
conflicted with upstream's replacement for the incremental active-room filter
(#696). The old custom filter was removed in favor of upstream's batched
publication/activity checks. Receipt notification cursors now use upstream's
transactional projection; custom initial/full-state unread counts, room tags,
invite-to-join state and prompt verification delivery remain in place.

Also on 2026-11-17, classic incremental sync stopped reloading every joined
room when the filter sets `unread_thread_notifications` (Element Web's stored
filter does). Upstream disabled the unchanged-room skip for that filter, so a
567-room account spent 6–13 seconds per pass and saw new messages 8–20 seconds
late. Every push-summary write now advances the per-room notification cursor
(`event_receipts_notification_cursors`) under the receipt stream fence. That
includes push actions evaluated after an event's publication and summary
rebuilds; receipts already did so. The existing batched cursor check therefore
proves that main and per-thread counts are unchanged, and the long-poll watcher
wakes on count-only changes. Publication-frame capture now probes each room's
newest publication through `(room_id, live_published_sn DESC)` and falls back to
the full scan only if that row is not accepted. Production data gave the same
567 rows in about 9 ms instead of about 220 ms.

Both release architectures must pass the existing HTTP Docker fixture before
publishing an image; the same fixture runs on every pull request into
`patched`.

Device-list tracking (`device_lists::changes`, shared by classic sync, sliding
sync and `/keys/changes`) no longer walks every room the user was ever in. It
used to look up each room's publication frame separately at both window
endpoints and re-decode each frame's membership on every pass. For a 569-room
account that was more than 2,000 sequential queries and 1.5–4.5 s per sync
pass. Now one LATERAL statement locates every candidate room's frame at both
endpoints through `event_points_room_live_publication`. On production data it
returns the same 569 rows in about 10 ms. Decoded frame membership, which is
immutable, is kept in a process-wide cache bounded by frames and member
pointers. Failed decodes are never cached, so missing member events still fail
closed. When no candidate room published new state in the window, the
membership comparison is skipped and only the existing key-change query runs.
A repeated or key-only window therefore costs two statements, whatever the
room count.

The default Sliding Sync build now sends new list positions and room data
when a client expands its room list during an idle poll. A count-only reply
used to tell Element X to advance without receiving those rooms. The server
can still send a count-only reply for an unchanged, acknowledged window.
The custom release fixture tests the growing window over HTTP without
advancing the event cursor. Room ordering uses one indexed event lookup per
candidate room to keep the idle comparison cheap.

On 2026-11-23 the fork merged upstream `50511ee0` (sliding-sync membership,
invitation and lazy-member changes #701–#706). The one conflict was the idle-room
skip in `sync_v5::process_rooms`: upstream's version is kept, together with the
custom read-count check that still sends a room whose unread counts changed
without new events. All `database_` tests of the server crate and the sliding
sync `review_` tests pass on the merge.
