//! Device tracking at immutable sync publication boundaries.
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, LazyLock, Mutex};

use diesel::prelude::*;
use horme::RunQueryDsl;
use lru_cache::LruCache;

use crate::core::device::DeviceLists;
use crate::core::events::StateEventType;
use crate::core::events::room::member::{MembershipState, RoomMemberEventContent};
use crate::core::{OwnedEventId, OwnedRoomId, OwnedUserId, Seqnum, UserId};
use crate::data::schema::*;
use crate::{AppResult, MatrixError, room};

#[cfg(test)]
mod batch_tests;
#[cfg(test)]
mod regression;
#[cfg(test)]
pub(crate) use regression::assert_large_custom_state_is_not_resolved;

#[derive(Default)]
struct Sharing {
    all: BTreeSet<OwnedUserId>,
    encrypted: BTreeSet<OwnedUserId>,
    joined_rooms: BTreeMap<OwnedRoomId, Seqnum>,
    encrypted_rooms: BTreeSet<OwnedRoomId>,
    room_members: BTreeMap<OwnedRoomId, Arc<BTreeMap<OwnedUserId, MemberPoint>>>,
}

struct MemberPoint {
    event_id: OwnedEventId,
    event_sn: Seqnum,
}

/// Joined members of one published state frame. Frames, their member
/// pointers and member event bodies are immutable, so the decoded form is
/// shared by every request and both endpoints of a window.
struct FrameMembers {
    joined: Arc<BTreeMap<OwnedUserId, MemberPoint>>,
    encrypted: bool,
}

/// Bounded by frames and by the total number of decoded member pointers.
/// A 567-room account needs ~570 frames and ~6,600 pointers per window.
const FRAME_CACHE_FRAMES: usize = 8_192;
const FRAME_CACHE_MEMBERS: usize = 250_000;

struct FrameCache {
    frames: LruCache<i64, Arc<FrameMembers>>,
    members: usize,
}

impl FrameCache {
    fn get(&mut self, frame: i64) -> Option<Arc<FrameMembers>> {
        self.frames.get_mut(&frame).cloned()
    }

    fn insert(&mut self, frame: i64, members: Arc<FrameMembers>) {
        let weight = members.joined.len() + 1;
        if weight > FRAME_CACHE_MEMBERS {
            return;
        }
        if let Some(old) = self.frames.remove(&frame) {
            self.members -= old.joined.len() + 1;
        }
        while self.frames.len() >= FRAME_CACHE_FRAMES || self.members + weight > FRAME_CACHE_MEMBERS
        {
            let Some((_, old)) = self.frames.remove_lru() else {
                break;
            };
            self.members -= old.joined.len() + 1;
        }
        self.members += weight;
        self.frames.insert(frame, members);
    }
}

static FRAME_CACHE: LazyLock<Mutex<FrameCache>> = LazyLock::new(|| {
    Mutex::new(FrameCache {
        // Eviction is explicit above; the extra slot keeps insert from
        // silently dropping an entry without adjusting the member weight.
        frames: LruCache::new(FRAME_CACHE_FRAMES + 1),
        members: 0,
    })
});

/// The newest publication frame of one candidate room before each endpoint.
struct RoomFrames {
    room_id: OwnedRoomId,
    before: Option<i64>,
    after: Option<i64>,
}

/// Locate the frames of every room the user was ever in, at both endpoints,
/// in one statement. Each side is a top-one probe of
/// `event_points_room_live_publication`, matching the former per-room
/// `live_published_sn < boundary ORDER BY live_published_sn DESC` lookup.
const BOUNDARY_FRAMES_QUERY: &str = "\
    SELECT r.room_id, \
        b.live_published_sn AS before_sn, b.live_published_frame_id AS before_frame, \
        a.live_published_sn AS after_sn, a.live_published_frame_id AS after_frame \
    FROM (SELECT DISTINCT room_id FROM room_users WHERE user_id = $1) AS r \
    LEFT JOIN LATERAL ( \
        SELECT p.live_published_sn, p.live_published_frame_id FROM event_points AS p \
        WHERE p.room_id = r.room_id AND p.live_published_sn IS NOT NULL \
          AND p.live_published_sn < $2 \
        ORDER BY p.live_published_sn DESC LIMIT 1 \
    ) AS b ON TRUE \
    LEFT JOIN LATERAL ( \
        SELECT p.live_published_sn, p.live_published_frame_id FROM event_points AS p \
        WHERE p.room_id = r.room_id AND p.live_published_sn IS NOT NULL \
          AND p.live_published_sn < $3 \
        ORDER BY p.live_published_sn DESC LIMIT 1 \
    ) AS a ON TRUE";

async fn boundary_frames(user: &UserId, from: Seqnum, to: Seqnum) -> AppResult<Vec<RoomFrames>> {
    use diesel::sql_types::{BigInt, Nullable, Text};

    #[derive(QueryableByName)]
    struct Row {
        #[diesel(sql_type = Text)]
        room_id: OwnedRoomId,
        #[diesel(sql_type = Nullable<BigInt>)]
        before_sn: Option<Seqnum>,
        #[diesel(sql_type = Nullable<BigInt>)]
        before_frame: Option<i64>,
        #[diesel(sql_type = Nullable<BigInt>)]
        after_sn: Option<Seqnum>,
        #[diesel(sql_type = Nullable<BigInt>)]
        after_frame: Option<i64>,
    }
    // Membership rows are retained on leave and forget. They locate candidate
    // rooms only: authorization comes from the immutable published frame.
    let rows = diesel::sql_query(BOUNDARY_FRAMES_QUERY)
        .bind::<Text, _>(user.as_str())
        .bind::<BigInt, _>(from)
        .bind::<BigInt, _>(to)
        .load::<Row>(&mut crate::data::db())
        .await?;
    let frame = |sn: Option<Seqnum>, frame: Option<i64>| match (sn, frame) {
        (None, _) => Ok(None),
        (Some(_), Some(frame)) => Ok(Some(frame)),
        (Some(_), None) => Err(MatrixError::unknown("published state frame missing")),
    };
    rows.into_iter()
        .map(|row| {
            Ok(RoomFrames {
                before: frame(row.before_sn, row.before_frame)?,
                after: frame(row.after_sn, row.after_frame)?,
                room_id: row.room_id,
            })
        })
        .collect()
}

async fn joined_period(
    user: &UserId,
    point: &MemberPoint,
    periods: &mut HashMap<OwnedEventId, Seqnum>,
) -> AppResult<Seqnum> {
    if let Some(period) = periods.get(&point.event_id) {
        return Ok(*period);
    }
    let period =
        room::user::join_sn_for_event(user, point.event_id.clone(), point.event_sn).await?;
    periods.insert(point.event_id.clone(), period);
    Ok(period)
}

async fn frame_members(room_id: &OwnedRoomId, frame: i64) -> AppResult<Arc<FrameMembers>> {
    if let Some(members) = FRAME_CACHE.lock().unwrap().get(frame) {
        return Ok(members);
    }
    // Decode field/event sequence numbers without resolving event IDs.
    // get_full_state_ids calls CompressedEvent::split for every entry,
    // which adds one event_points lookup per unrelated custom event.
    let layers = room::state::load_frame_info(frame).await?;
    let layer = layers
        .last()
        .ok_or_else(|| MatrixError::unknown("published state frame missing"))?;
    let state: HashMap<_, _> = layer
        .full_state
        .iter()
        .map(|event| (event.field_id(), event.event_sn()))
        .collect();
    let field_ids: Vec<_> = state.keys().copied().collect();
    let mut joined = BTreeMap::new();
    let mut encrypted = false;
    // Restrict field metadata before loading event bodies: unrelated
    // custom state never adds PDU reads. Batch bodies; failures are never
    // cached, so a missing selected member keeps failing closed.
    for ids in field_ids.chunks(1_000) {
        let fields = room_state_fields::table
            .filter(room_state_fields::id.eq_any(ids))
            .filter(room_state_fields::event_ty.eq_any(["m.room.member", "m.room.encryption"]))
            .load::<crate::data::room::DbRoomStateField>(&mut crate::data::db())
            .await?;
        let members: HashMap<_, _> = fields
            .into_iter()
            .filter_map(|field| {
                if field.event_ty == StateEventType::RoomEncryption {
                    encrypted |= field.state_key.is_empty();
                    None
                } else {
                    Some((state[&field.id], field.state_key))
                }
            })
            .collect();
        if members.is_empty() {
            continue;
        }
        // event_points has a (room_id, event_sn) index; event_datas
        // has an event_id primary key, but no event_sn-only index.
        // Resolve only selected memberships in one indexed batch join.
        let bodies = event_points::table
            .inner_join(
                event_datas::table.on(event_datas::event_id
                    .eq(event_points::event_id)
                    .and(event_datas::event_sn.eq(event_points::event_sn))),
            )
            .filter(event_points::room_id.eq(room_id))
            .filter(event_points::event_sn.eq_any(members.keys()))
            .filter(event_datas::room_id.eq(room_id))
            .select((
                event_points::event_sn,
                event_points::event_id,
                event_datas::json_data,
            ))
            .load::<(Seqnum, crate::core::OwnedEventId, serde_json::Value)>(&mut crate::data::db())
            .await?;
        if bodies.len() != members.len() {
            return Err(MatrixError::unknown("published member event missing").into());
        }
        for (event_sn, event_id, body) in bodies {
            let content: RoomMemberEventContent = serde_json::from_value(body["content"].clone())?;
            if content.membership == MembershipState::Join {
                let member = UserId::parse(&members[&event_sn])?;
                joined.insert(member, MemberPoint { event_id, event_sn });
            }
        }
    }
    let members = Arc::new(FrameMembers {
        joined: Arc::new(joined),
        encrypted,
    });
    FRAME_CACHE
        .lock()
        .unwrap()
        .insert(frame, Arc::clone(&members));
    Ok(members)
}

/// Decode each distinct frame once per request; repeated windows hit the
/// process-wide cache and issue no per-room queries.
async fn load_frames(rooms: &[RoomFrames]) -> AppResult<HashMap<i64, Arc<FrameMembers>>> {
    let mut frames = HashMap::new();
    for room in rooms {
        for frame in [room.before, room.after].into_iter().flatten() {
            if let std::collections::hash_map::Entry::Vacant(entry) = frames.entry(frame) {
                entry.insert(frame_members(&room.room_id, frame).await?);
            }
        }
    }
    Ok(frames)
}

async fn sharing_at(
    user: &UserId,
    rooms: impl Iterator<Item = (&OwnedRoomId, Option<i64>)>,
    frames: &HashMap<i64, Arc<FrameMembers>>,
    join_periods: &mut HashMap<OwnedEventId, Seqnum>,
) -> AppResult<Sharing> {
    let mut sharing = Sharing::default();
    for (room_id, frame) in rooms {
        let Some(frame) = frame else { continue };
        let members = &frames[&frame];
        let Some(point) = members.joined.get(user) else {
            continue;
        };
        // A profile-only member update retains the same joined period;
        // leave+rejoin starts a new one even if both endpoints contain this
        // same room.
        let period = joined_period(user, point, join_periods).await?;
        sharing.joined_rooms.insert(room_id.clone(), period);
        sharing
            .room_members
            .insert(room_id.clone(), Arc::clone(&members.joined));
        sharing.all.extend(members.joined.keys().cloned());
        if members.encrypted {
            sharing.encrypted_rooms.insert(room_id.clone());
            sharing.encrypted.extend(members.joined.keys().cloned());
        }
    }
    sharing.all.remove(user);
    sharing.encrypted.remove(user);
    Ok(sharing)
}

/// Device and key updates in `[from, to)` by peers sharing a room at `to`,
/// and by the user's own devices.
async fn key_changes(
    user: &UserId,
    after: &Sharing,
    from: Seqnum,
    to: Seqnum,
) -> AppResult<BTreeSet<OwnedUserId>> {
    let candidates: Vec<_> = after
        .all
        .iter()
        .cloned()
        .chain(std::iter::once(user.to_owned()))
        .collect();
    // Device key updates are relevant in plaintext shared rooms too. A
    // user's own devices remain tracked independently of room membership.
    Ok(
        crate::data::user::key::keys_changed_in_window(&candidates, from, to)
            .await?
            .into_iter()
            .filter(|changed| changed == user || after.all.contains(changed))
            .collect(),
    )
}

/// `from` and `to` are next-sync positions: changes occupy [from, to).
/// The same calculation serves keys/changes, classic sync and sliding sync.
pub(crate) async fn changes(user: &UserId, from: Seqnum, to: Seqnum) -> AppResult<DeviceLists> {
    if from < 0 || to < from {
        return Err(MatrixError::invalid_param("invalid device-list sync interval").into());
    }
    if from == to {
        return Ok(DeviceLists::default());
    }
    let rooms = boundary_frames(user, from, to).await?;
    let frames = load_frames(&rooms).await?;
    let mut join_periods = HashMap::new();
    let after = sharing_at(
        user,
        rooms.iter().map(|room| (&room.room_id, room.after)),
        &frames,
        &mut join_periods,
    )
    .await?;
    // Ordinary polls publish no state in any of the user's rooms. Identical
    // frames give identical sharing, members and joined periods at both
    // endpoints, so only device and key updates can be reported.
    if rooms.iter().all(|room| room.before == room.after) {
        return Ok(DeviceLists {
            changed: key_changes(user, &after, from, to)
                .await?
                .into_iter()
                .collect(),
            left: Vec::new(),
        });
    }
    let before = sharing_at(
        user,
        rooms.iter().map(|room| (&room.room_id, room.before)),
        &frames,
        &mut join_periods,
    )
    .await?;
    // Preserve the existing sync notification for newly shared plaintext
    // peers too. Enabling encryption in an existing room starts E2EE tracking
    // even when the users already shared that plaintext room.
    let mut changed: BTreeSet<_> = after.all.difference(&before.all).cloned().collect();
    changed.extend(after.encrypted.difference(&before.encrypted).cloned());
    // Endpoint user sets alone cannot detect leave+rejoin between two syncs.
    // First prove continuity from unchanged member identities in any shared
    // room. Only then walk changed peer identities that have no other proof;
    // ordinary polls and unrelated state never walk every peer's join history.
    let mut continuous = BTreeSet::new();
    let mut changed_members = Vec::new();
    for (room_id, before_members) in &before.room_members {
        if before.joined_rooms.get(room_id) != after.joined_rooms.get(room_id) {
            continue;
        }
        let Some(after_members) = after.room_members.get(room_id) else {
            continue;
        };
        for (peer, old) in before_members.iter() {
            if peer == user {
                continue;
            }
            let Some(new) = after_members.get(peer) else {
                continue;
            };
            if old.event_id == new.event_id {
                continuous.insert(peer.clone());
            } else {
                changed_members.push((peer, old, new));
            }
        }
    }
    for (peer, old, new) in changed_members {
        if continuous.contains(peer) {
            continue;
        }
        if joined_period(peer, old, &mut join_periods).await?
            == joined_period(peer, new, &mut join_periods).await?
        {
            continuous.insert(peer.clone());
        }
    }
    changed.extend(
        before
            .all
            .intersection(&after.all)
            .filter(|peer| !continuous.contains(*peer))
            .cloned(),
    );
    // An imported/federated join can publish membership without uploading a
    // device key or writing an own-user key marker. Other devices must still
    // learn about this user's newly joined room. Keep that notification apart
    // from peer tracking so the caller is never reported in `left`.
    if after
        .joined_rooms
        .iter()
        .any(|(room, period)| before.joined_rooms.get(room) != Some(period))
        || !after.encrypted_rooms.is_subset(&before.encrypted_rooms)
    {
        changed.insert(user.to_owned());
    }
    changed.extend(key_changes(user, &after, from, to).await?);
    let mut left: BTreeSet<_> = before
        .encrypted
        .difference(&after.encrypted)
        .cloned()
        .collect();
    left.extend(before.all.difference(&after.all).cloned());
    Ok(DeviceLists {
        changed: changed.into_iter().collect(),
        left: left.into_iter().collect(),
    })
}
