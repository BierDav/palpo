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

The scheduled upstream sync records each new base revision and replays only
the remaining custom commits. Conflicts stop the workflow with a job summary;
the fork's GitHub issues are disabled. Custom release notes use the pinned
revision so upstream history is not listed as custom patches.

The isolated HTTP regressions for room preferences and invite delivery are
registered in default/all-feature database CI. The custom Docker fixture also
checks standard tag bodies, account-data echoes and cleared unread counters on
both release architectures.
