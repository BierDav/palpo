# Custom fork

`BierDav/palpo` builds the `patched` branch for `quickme.at`. The upstream base
is pinned by `.custom-upstream-revision`; `.custom-upstream-tag` supplies the
release version used in custom image tags. These have different purposes:
the fork now follows upstream `main`, rather than the latest stable tag.

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

The scheduled upstream sync records each new base revision and merges upstream
into the complete fork history without rewriting it. Conflicts abort the merge
and stop the workflow with a job summary;
the fork's GitHub issues are disabled. Custom release notes use the pinned
revision so upstream history is not listed as custom patches.

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

The sync script preserves mixed source/CI commits through normal merges and
uses a fast-forward push, so concurrent fork updates cannot be overwritten.
Its disposable-repository tests cover preserved custom history, a no-op sync,
conflict cleanup and a competing writer. Changes to the sync workflow/script
run these checks and the sync immediately; ordinary custom commits still use
the custom release workflow. Both release architectures must pass the existing
HTTP Docker fixture before publishing an image.

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
