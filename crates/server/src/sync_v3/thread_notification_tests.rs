use std::time::Duration;

use salvo::prelude::Service;
use salvo::test::TestClient;
use serde_json::{Value, json};

use super::resource_review_tests::{request, setup};
use super::*;

/// Element Web's stored sync filter.
const ELEMENT_FILTER: &str = r#"{"room":{"state":{"lazy_load_members":true},"timeline":{"unread_thread_notifications":true}}}"#;

#[test]
#[ignore = "requires an empty dedicated PALPO_TEST_DATABASE_URL; run alone"]
fn review_thread_notification_filter_skips_unchanged_rooms_and_delivers_thread_counts() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(thread_counts());
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
#[ignore = "requires an empty dedicated PALPO_TEST_DATABASE_URL; run alone"]
fn review_lateral_frame_capture_matches_distinct_on_publication_frames() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(lateral_frames());
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn sync_failing(
    sender: &UserId,
    fail: &RoomId,
    args: &SyncEventsReqArgs,
) -> AppResult<SyncEventsResBody> {
    *joined_room_failure_tests::FAIL_ROOM.lock().unwrap() = Some(fail.to_owned());
    let response = sync_events(sender, "D".into(), args).await;
    *joined_room_failure_tests::FAIL_ROOM.lock().unwrap() = None;
    response
}

/// `changed` must enter its payload loader, while `quiet` must be skipped.
async fn sync_delivers(
    sender: &UserId,
    changed: &RoomId,
    quiet: &RoomId,
    args: &mut SyncEventsReqArgs,
) -> Value {
    assert!(
        sync_failing(sender, changed, args).await.is_err(),
        "changed room was incorrectly skipped"
    );
    let response = sync_failing(sender, quiet, args)
        .await
        .expect("unchanged room entered its payload loader");
    args.since = Some(response.next_batch.clone());
    serde_json::to_value(response).unwrap()["rooms"]["join"][changed.as_str()].clone()
}

/// A non-empty window without room activity loads no room or summary.
async fn sync_idle(sender: &UserId, room: &RoomId, args: &mut SyncEventsReqArgs) {
    data::next_sn().await.unwrap();
    let spy = crate::test_database::sql_spy::SqlSpy::install().await;
    let response = sync_failing(sender, room, args)
        .await
        .expect("unchanged room entered its payload loader under unread_thread_notifications");
    assert!(response.rooms.join.is_empty());
    assert!(
        !spy.snapshot()
            .iter()
            .any(|q| q.contains("event_push_summaries")),
        "quiet sync loaded per-room notification summaries"
    );
    spy.assert_coverage().await;
    spy.uninstall().await;
    args.since = Some(response.next_batch);
}

fn thread_count(room: &Value, root: &str) -> Value {
    room["unread_thread_notifications"][root]["notification_count"].clone()
}

async fn send(service: &Service, room: &RoomId, txn: &str, content: Value) -> OwnedEventId {
    let sent = request(
        TestClient::put(format!(
            "http://localhost/_matrix/client/v3/rooms/{room}/send/m.room.message/{txn}"
        ))
        .add_header("Authorization", "Bearer bob", true)
        .json(&content),
        service,
    )
    .await;
    sent["event_id"].as_str().unwrap().try_into().unwrap()
}

fn reply(root: &EventId, body: &str) -> Value {
    json!({"msgtype":"m.text","body":body,"m.relates_to":{
        "rel_type":"m.thread","event_id":root,"is_falling_back":true,
        "m.in_reply_to":{"event_id":root}
    }})
}

async fn thread_counts() {
    use diesel::sql_types::Text;
    use horme::RunQueryDsl;
    let (service, room, sender, _peer) = setup().await;
    let quiet: OwnedRoomId = request(
        TestClient::post("http://localhost/_matrix/client/v3/createRoom")
            .add_header("Authorization", "Bearer alice", true)
            .json(&json!({"room_version":"11","preset":"public_chat"})),
        &service,
    )
    .await["room_id"]
        .as_str()
        .unwrap()
        .try_into()
        .unwrap();
    let initial = sync_events(
        &sender,
        "D".into(),
        &serde_json::from_value(json!({"filter":ELEMENT_FILTER})).unwrap(),
    )
    .await
    .unwrap();
    let mut args: SyncEventsReqArgs =
        serde_json::from_value(json!({"since":initial.next_batch,"filter":ELEMENT_FILTER}))
            .unwrap();
    sync_idle(&sender, &room, &mut args).await;

    let root = send(
        &service,
        &room,
        "root",
        json!({"msgtype":"m.text","body":"root"}),
    )
    .await;
    let delivered = sync_delivers(&sender, &room, &quiet, &mut args).await;
    assert!(
        delivered["timeline"]["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["event_id"] == root.as_str())
    );
    assert_eq!(delivered["unread_notifications"]["notification_count"], 1);
    // A room with notification rows is still skipped once nothing changes.
    sync_idle(&sender, &room, &mut args).await;

    send(&service, &room, "reply-1", reply(&root, "first")).await;
    let delivered = sync_delivers(&sender, &room, &quiet, &mut args).await;
    assert_eq!(thread_count(&delivered, root.as_str()), 1);
    assert_eq!(
        delivered["unread_thread_notifications"][root.as_str()]["highlight_count"],
        0
    );
    // With unread_thread_notifications the room count is main-timeline only.
    assert_eq!(delivered["unread_notifications"]["notification_count"], 1);
    sync_idle(&sender, &room, &mut args).await;

    // Push actions are evaluated after an event's live publication. Simulate a
    // sync that delivered the threaded event before its action existed.
    let second = send(&service, &room, "reply-2", reply(&root, "second")).await;
    diesel::sql_query("DELETE FROM event_push_actions WHERE user_id=$1 AND event_id=$2")
        .bind::<Text, _>(sender.as_str())
        .bind::<Text, _>(second.as_str())
        .execute(&mut data::db())
        .await
        .unwrap();
    diesel::sql_query("UPDATE event_push_summaries SET notification_count=notification_count-1 WHERE user_id=$1 AND room_id=$2 AND thread_id=$3")
        .bind::<Text, _>(sender.as_str())
        .bind::<Text, _>(room.as_str())
        .bind::<Text, _>(root.as_str())
        .execute(&mut data::db())
        .await
        .unwrap();
    let delivered = sync_delivers(&sender, &room, &quiet, &mut args).await;
    assert!(
        delivered["timeline"]["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["event_id"] == second.as_str())
    );
    assert_eq!(thread_count(&delivered, root.as_str()), 1);
    sync_idle(&sender, &room, &mut args).await;
    let baseline = crate::watcher::load_watch_snapshot(&sender, "D".into(), false)
        .await
        .unwrap();
    room::push_action::upsert_push_action(&room, &second, &sender, true, false)
        .await
        .unwrap();
    tokio::time::timeout(
        Duration::from_secs(10),
        crate::watcher::watch_from(&sender, "D".into(), false, None, baseline),
    )
    .await
    .expect("a late count change must wake the long poll")
    .unwrap();
    let delivered = sync_delivers(&sender, &room, &quiet, &mut args).await;
    assert_eq!(thread_count(&delivered, root.as_str()), 2);
    assert!(
        delivered["timeline"]["events"]
            .as_array()
            .is_none_or(|events| events.is_empty()),
        "count-only change repeated timeline events"
    );
    sync_idle(&sender, &room, &mut args).await;

    // A threaded read receipt clears only that thread.
    request(
        TestClient::post(format!(
            "http://localhost/_matrix/client/v3/rooms/{room}/receipt/m.read/{second}"
        ))
        .add_header("Authorization", "Bearer alice", true)
        .json(&json!({"thread_id":root})),
        &service,
    )
    .await;
    let delivered = sync_delivers(&sender, &room, &quiet, &mut args).await;
    assert!(
        thread_count(&delivered, root.as_str()).is_null()
            || thread_count(&delivered, root.as_str()) == 0,
        "{delivered}"
    );
    assert_eq!(delivered["unread_notifications"]["notification_count"], 1);
    sync_idle(&sender, &room, &mut args).await;

    // A private threaded receipt is invisible to others but still changes counts.
    let third = send(&service, &room, "reply-3", reply(&root, "third")).await;
    let delivered = sync_delivers(&sender, &room, &quiet, &mut args).await;
    assert_eq!(thread_count(&delivered, root.as_str()), 1);
    request(
        TestClient::post(format!(
            "http://localhost/_matrix/client/v3/rooms/{room}/receipt/m.read.private/{third}"
        ))
        .add_header("Authorization", "Bearer alice", true)
        .json(&json!({"thread_id":root})),
        &service,
    )
    .await;
    let delivered = sync_delivers(&sender, &room, &quiet, &mut args).await;
    assert!(
        thread_count(&delivered, root.as_str()).is_null()
            || thread_count(&delivered, root.as_str()) == 0
    );
    sync_idle(&sender, &room, &mut args).await;

    count_change_publication_fence(&sender, &room).await;
}

/// Count changes allocate from the receipt stream and hold its shared lock, so
/// a sync token cannot pass an uncommitted change.
async fn count_change_publication_fence(user: &UserId, room: &RoomId) {
    use diesel::sql_types::BigInt;
    use horme::RunQueryDsl;
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let (writer_user, writer_room) = (user.to_owned(), room.to_owned());
    let writer = tokio::spawn(async move {
        data::db()
            .transaction::<_, data::DataError, _>(async |conn| {
                data::room::receipt::publish_notification_change_with_conn(
                    conn,
                    &writer_user,
                    &writer_room,
                )
                .await?;
                let sn = diesel::select(diesel::dsl::sql::<BigInt>("currval('occur_sn_seq')"))
                    .get_result::<Seqnum>(conn)
                    .await?;
                ready_tx.send(sn).unwrap();
                release_rx.await.unwrap();
                Ok(())
            })
            .await
    });
    let published = tokio::time::timeout(Duration::from_secs(4), ready_rx)
        .await
        .unwrap()
        .unwrap();
    let snapshot_user = user.to_owned();
    let mut snapshot = tokio::spawn(async move {
        crate::event::sticky::curr_sn_after_sync_writes(&snapshot_user, "D".into())
            .await
            .unwrap()
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(500), &mut snapshot)
            .await
            .is_err(),
        "sync snapshot crossed an uncommitted notification count change"
    );
    release_tx.send(()).unwrap();
    writer.await.unwrap().unwrap();
    assert!(snapshot.await.unwrap() >= published);
    assert_eq!(
        room::user::last_read_notification(user, room)
            .await
            .unwrap(),
        published
    );
}

#[derive(diesel::QueryableByName, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct LegacyFrame {
    #[diesel(sql_type = diesel::sql_types::Text)]
    room_id: String,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    frame_id: i64,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    sn: i64,
}

/// The previous DISTINCT ON implementation, kept as the reference result.
async fn legacy_frames(rooms: &[OwnedRoomId], until: Seqnum) -> Vec<LegacyFrame> {
    use diesel::sql_types::{Array, BigInt, Text};
    use horme::RunQueryDsl;
    let mut rows = diesel::sql_query(
        "SELECT DISTINCT ON (point.room_id) point.room_id, \
         point.live_published_frame_id AS frame_id, point.live_published_sn AS sn \
         FROM event_points AS point JOIN events AS event \
         ON event.id = point.event_id AND event.room_id = point.room_id \
         WHERE point.room_id = ANY($1) AND point.live_published_frame_id IS NOT NULL \
         AND NOT event.is_outlier AND NOT event.soft_failed AND NOT event.is_rejected \
         AND event.stream_ordering >= 0 AND point.live_published_sn <= $2 \
         ORDER BY point.room_id, point.live_published_sn DESC",
    )
    .bind::<Array<Text>, _>(rooms.iter().map(|room| room.as_str()).collect::<Vec<_>>())
    .bind::<BigInt, _>(until)
    .load::<LegacyFrame>(&mut data::db())
    .await
    .unwrap();
    rows.sort();
    rows
}

async fn assert_frames_match(rooms: &[OwnedRoomId], untils: impl Iterator<Item = Seqnum>) {
    for until in untils {
        let mut lateral = captured_sync_frames(rooms, until)
            .await
            .unwrap()
            .into_iter()
            .map(|(room_id, p)| LegacyFrame {
                room_id: room_id.to_string(),
                frame_id: p.frame_id,
                sn: p.sn,
            })
            .collect::<Vec<_>>();
        lateral.sort();
        assert_eq!(lateral, legacy_frames(rooms, until).await, "until={until}");
    }
}

async fn lateral_frames() {
    use diesel::sql_types::Text;
    use horme::RunQueryDsl;
    let (service, room, sender, _peer) = setup().await;
    for i in 0..3 {
        let created: OwnedRoomId = request(
            TestClient::post("http://localhost/_matrix/client/v3/createRoom")
                .add_header("Authorization", "Bearer alice", true)
                .json(&json!({"room_version":"11","preset":"public_chat"})),
            &service,
        )
        .await["room_id"]
            .as_str()
            .unwrap()
            .try_into()
            .unwrap();
        request(
            TestClient::put(format!(
                "http://localhost/_matrix/client/v3/rooms/{created}/state/m.room.topic"
            ))
            .add_header("Authorization", "Bearer alice", true)
            .json(&json!({"topic":format!("topic {i}")})),
            &service,
        )
        .await;
    }
    for i in 0..4 {
        send(
            &service,
            &room,
            &format!("frame-{i}"),
            json!({"msgtype":"m.text","body":"frame"}),
        )
        .await;
    }
    let mut rooms = data::user::joined_rooms(&sender).await.unwrap();
    rooms.push(RoomId::parse("!missing:filtered.example").unwrap());
    let end = data::curr_sn().await.unwrap();
    assert_frames_match(&rooms, 1..=end).await;
    // Duplicates and multiple chunks keep one result per room.
    let repeated: Vec<_> = rooms.iter().cycle().take(600).cloned().collect();
    assert_frames_match(&repeated, [end].into_iter()).await;

    // Excluded publications fall back to the previous accepted one in both.
    #[derive(diesel::QueryableByName)]
    struct Latest {
        #[diesel(sql_type = Text)]
        event_id: String,
    }
    let latest = diesel::sql_query(
        "SELECT event_id FROM event_points WHERE room_id=$1 \
         AND live_published_sn IS NOT NULL ORDER BY live_published_sn DESC LIMIT 1",
    )
    .bind::<Text, _>(room.as_str())
    .get_result::<Latest>(&mut data::db())
    .await
    .unwrap()
    .event_id;
    for (set, reset) in [
        ("is_outlier = true", "is_outlier = false"),
        ("soft_failed = true", "soft_failed = false"),
        ("is_rejected = true", "is_rejected = false"),
        (
            "stream_ordering = -stream_ordering",
            "stream_ordering = -stream_ordering",
        ),
    ] {
        for update in [set, reset] {
            diesel::sql_query(format!("UPDATE events SET {update} WHERE id=$1"))
                .bind::<Text, _>(&latest)
                .execute(&mut data::db())
                .await
                .unwrap();
            assert_frames_match(&rooms, [end, end.saturating_sub(1)].into_iter()).await;
        }
    }
}
