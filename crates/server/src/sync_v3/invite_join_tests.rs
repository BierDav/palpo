//! Real client requests against an empty, dedicated PostgreSQL fixture.
use salvo::prelude::*;
use salvo::test::{RequestBuilder, ResponseExt, TestClient};
use serde_json::{Value, json};

use crate::core::{OwnedDeviceId, UnixMillis, UserId};
use crate::{config, data};

const SERVER: &str = "invite-join.test";
const ALICE: &str = "@alice:invite-join.test";
const SLIDING: &str =
    "http://localhost/_matrix/client/unstable/org.matrix.simplified_msc3575/sync?timeout=0";

async fn request(builder: RequestBuilder, service: &Service, token: &str) -> Value {
    let router = service.router();
    let builder = builder.add_header("Authorization", format!("Bearer {token}"), true);
    let mut response = tokio::spawn(async move { builder.send(router).await })
        .await
        .unwrap();
    let body: Value = response.take_json().await.unwrap();
    assert_eq!(response.status_code, Some(StatusCode::OK), "{body}");
    body
}

async fn setup() -> Service {
    crate::test_database::init();
    config::CONFIG
        .set(
            serde_json::from_value(json!({
                "server_name": SERVER, "db": {"url": "unused-test-config"},
                "ip_range_denylist": [], "auto_join_rooms": []
            }))
            .unwrap(),
        )
        .unwrap();
    for name in ["alice", "bob"] {
        let user = UserId::parse(format!("@{name}:{SERVER}")).unwrap();
        data::user::create_user(&data::user::NewDbUser {
            id: user.clone(),
            ty: None,
            is_admin: false,
            is_guest: false,
            is_local: true,
            localpart: name.into(),
            server_name: user.server_name().to_owned(),
            appservice_id: None,
            created_at: UnixMillis::now(),
        })
        .await
        .unwrap();
        data::user::device::create_device(&user, &OwnedDeviceId::from("D"), name, None, None)
            .await
            .unwrap();
        crate::user::get_push_rules(&user).await.unwrap();
    }
    Service::new(crate::routing::root())
}

async fn post(service: &Service, path: &str, body: Value, token: &str) -> Value {
    request(
        TestClient::post(format!("http://localhost/_matrix/client/v3/{path}")).json(&body),
        service,
        token,
    )
    .await
}

async fn invited_room(service: &Service) -> String {
    let room = post(service, "createRoom", json!({"name": "before"}), "bob").await;
    let room = room["room_id"].as_str().unwrap().to_owned();
    post(
        service,
        &format!("rooms/{room}/invite"),
        json!({"user_id": ALICE}),
        "bob",
    )
    .await;
    room
}

async fn classic(service: &Service, room: &str, since: Option<&str>, timeline: Value) -> Value {
    let mut url = url::Url::parse("http://localhost/_matrix/client/v3/sync").unwrap();
    url.query_pairs_mut()
        .append_pair("timeout", "0")
        .append_pair(
            "filter",
            &json!({
                "room": {"rooms": [room], "timeline": timeline,
                         "state": {"lazy_load_members": true}}
            })
            .to_string(),
        );
    if let Some(since) = since {
        url.query_pairs_mut().append_pair("since", since);
    }
    request(TestClient::get(url.as_str()), service, "alice").await
}

fn own_membership(events: &Value) -> Option<&str> {
    events
        .as_array()
        .into_iter()
        .flatten()
        .filter(|event| event["type"] == "m.room.member" && event["state_key"] == ALICE)
        .last()
        .and_then(|event| event["content"]["membership"].as_str())
}

fn run_test(future: impl Future<Output = ()> + Send + 'static) {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(future);
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
#[ignore = "requires an empty dedicated PALPO_TEST_DATABASE_URL; run this test alone"]
fn http_classic_invite_join_at_returned_timeline_boundary() {
    run_test(async {
        let service = setup().await;
        for timeline in [
            json!({"limit": 0}),
            json!({"limit": 10, "types": ["m.room.message"]}),
            json!({"limit": 1}),
        ] {
            let room = invited_room(&service).await;
            let invited = classic(&service, &room, None, json!({"limit": 10})).await;
            assert!(invited["rooms"]["invite"].get(&room).is_some(), "{invited}");
            post(&service, &format!("join/{room}"), json!({}), "alice").await;
            post(
                &service,
                &format!("rooms/{room}/send/m.room.message"),
                json!({"msgtype": "m.text", "body": "after join"}),
                "bob",
            )
            .await;
            let synced = classic(
                &service,
                &room,
                invited["next_batch"].as_str(),
                timeline.clone(),
            )
            .await;
            assert!(synced["rooms"]["invite"].get(&room).is_none(), "{synced}");
            let joined = &synced["rooms"]["join"][&room];
            assert_eq!(
                own_membership(&joined["state"]["events"]),
                Some("join"),
                "timeline {timeline}: {synced}"
            );
            let events = joined["timeline"]["events"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            assert!(events.iter().all(|event| event["type"] == "m.room.message"));
            assert_eq!(events.len(), if timeline["limit"] == 0 { 0 } else { 1 });
            // Initial zero-timeline sync also needs the complete joined state.
            let initial = classic(&service, &room, None, json!({"limit": 0})).await;
            assert_eq!(
                own_membership(&initial["rooms"]["join"][&room]["state"]["events"]),
                Some("join"),
                "{initial}"
            );
        }

        // The state block precedes the returned timeline. When it includes
        // historical pre-join events, don't put a future name or join ahead of them.
        let room = invited_room(&service).await;
        post(
            &service,
            &format!("rooms/{room}/send/m.room.message"),
            json!({"msgtype": "m.text", "body": "before join"}),
            "bob",
        )
        .await;
        post(&service, &format!("join/{room}"), json!({}), "alice").await;
        request(
            TestClient::put(format!(
                "http://localhost/_matrix/client/v3/rooms/{room}/state/m.room.name"
            ))
            .json(&json!({"name": "after"})),
            &service,
            "bob",
        )
        .await;
        let synced = classic(&service, &room, None, json!({"limit": 3})).await;
        let joined = &synced["rooms"]["join"][&room];
        assert_eq!(
            joined["timeline"]["events"][0]["content"]["body"], "before join",
            "{synced}"
        );
        assert_eq!(
            own_membership(&joined["state"]["events"]),
            Some("invite"),
            "{synced}"
        );
        assert_eq!(
            own_membership(&joined["timeline"]["events"]),
            Some("join"),
            "{synced}"
        );
        let name = joined["state"]["events"]
            .as_array()
            .unwrap()
            .iter()
            .find(|event| event["type"] == "m.room.name")
            .unwrap();
        assert_eq!(name["content"]["name"], "before", "{synced}");

        // Upgraded databases can have legacy own joins without before-frames.
        // Their mutable frame must not move future state before the join either.
        let room = invited_room(&service).await;
        let invited = classic(&service, &room, None, json!({"limit": 0})).await;
        post(&service, &format!("join/{room}"), json!({}), "alice").await;
        request(
            TestClient::put(format!(
                "http://localhost/_matrix/client/v3/rooms/{room}/state/m.room.name"
            ))
            .json(&json!({"name": "future legacy name"})),
            &service,
            "bob",
        )
        .await;
        let room_id: &crate::core::RoomId = room.as_str().try_into().unwrap();
        let member = crate::room::get_state(
            room_id,
            &crate::core::events::StateEventType::RoomMember,
            ALICE,
            None,
        )
        .await
        .unwrap();
        {
            use diesel_async::RunQueryDsl;
            diesel::sql_query(
                "UPDATE event_points SET before_frame_id=NULL, frame_id=$1 WHERE event_id=$2",
            )
            .bind::<diesel::sql_types::BigInt, _>(
                crate::room::get_frame_id(room_id, None).await.unwrap(),
            )
            .bind::<diesel::sql_types::Text, _>(member.event_id.as_str())
            .execute(&mut data::connect().await.unwrap())
            .await
            .unwrap();
        }
        let synced = classic(
            &service,
            &room,
            invited["next_batch"].as_str(),
            json!({"limit": 1, "types": ["m.room.member"]}),
        )
        .await;
        let joined = &synced["rooms"]["join"][&room];
        assert_eq!(
            own_membership(&joined["timeline"]["events"]),
            Some("join"),
            "{synced}"
        );
        assert!(joined["state"]["events"].as_array().unwrap().iter()
            .any(|event| event["type"] == "m.room.name" && event["content"]["name"] == "before"), "{synced}");
    });
}

#[test]
#[ignore = "requires an empty dedicated PALPO_TEST_DATABASE_URL; run this test alone"]
fn http_sliding_invite_join_and_required_state_without_timeline() {
    run_test(async {
        let service = setup().await;
        let room = invited_room(&service).await;
        let body = json!({"conn_id": "invite-join", "room_subscriptions": {
            &room: {"timeline_limit": 0, "required_state": [["m.room.member", "$ME"], ["m.room.name", ""]]}
        }});
        let first = request(TestClient::post(SLIDING).json(&body), &service, "alice").await;
        assert!(first["rooms"][&room]["invite_state"].is_array(), "{first}");
        // A second invite poll gives known_rooms a nonzero room_since_sn.
        // Joining immediately after the first poll would mask the early exit.
        // Unrelated traffic bypasses the ahead-of-stream idle fast path.
        post(&service, "createRoom", json!({}), "bob").await;
        let second = request(
            TestClient::post(format!("{SLIDING}&pos={}", first["pos"].as_str().unwrap()))
                .json(&body),
            &service,
            "alice",
        )
        .await;
        post(&service, &format!("join/{room}"), json!({}), "alice").await;
        let joined = request(
            TestClient::post(format!("{SLIDING}&pos={}", second["pos"].as_str().unwrap()))
                .json(&body),
            &service,
            "alice",
        )
        .await;
        assert_eq!(
            own_membership(&joined["rooms"][&room]["required_state"]),
            Some("join"),
            "{joined}"
        );
        assert!(
            joined["rooms"][&room]["timeline"]
                .as_array()
                .is_none_or(Vec::is_empty),
            "{joined}"
        );
        assert!(
            joined["rooms"][&room].get("invite_state").is_none(),
            "{joined}"
        );
        let pos = joined["pos"].as_str().unwrap();
        request(
            TestClient::put(format!(
                "http://localhost/_matrix/client/v3/rooms/{room}/state/m.room.name"
            ))
            .json(&json!({"name": "changed without timeline"})),
            &service,
            "bob",
        )
        .await;
        let changed = request(
            TestClient::post(format!("{SLIDING}&pos={pos}")).json(&body),
            &service,
            "alice",
        )
        .await;
        assert!(
            changed["rooms"][&room]["required_state"]
                .as_array()
                .unwrap()
                .iter()
                .any(|event| event["content"]["name"] == "changed without timeline"),
            "{changed}"
        );
        assert!(
            changed["rooms"][&room]["timeline"]
                .as_array()
                .is_none_or(Vec::is_empty),
            "{changed}"
        );
        // Force the ordinary incremental path with traffic in an unrelated room.
        post(&service, "createRoom", json!({}), "bob").await;
        let idle = request(
            TestClient::post(format!(
                "{SLIDING}&pos={}",
                changed["pos"].as_str().unwrap()
            ))
            .json(&body),
            &service,
            "alice",
        )
        .await;
        assert!(idle["rooms"].get(&room).is_none(), "{idle}");
    });
}
