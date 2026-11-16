//! Real Matrix routes against an empty dedicated PostgreSQL database.
use salvo::prelude::*;
use salvo::test::{RequestBuilder, ResponseExt, TestClient};
use serde_json::{Value, json};

use crate::core::{OwnedDeviceId, UnixMillis, UserId};
use crate::{config, data};

const ROOT: &str = "http://localhost/_matrix/client/v3";
const ALICE: &str = "@alice:preferences.test";

fn run(future: impl Future<Output = ()> + Send + 'static) {
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

async fn setup() -> Service {
    crate::test_database::init();
    config::CONFIG
        .set(
            serde_json::from_value(json!({
                "server_name":"preferences.test", "db":{"url":"unused-test-config"},
                "ip_range_denylist":[], "auto_join_rooms":[]
            }))
            .unwrap(),
        )
        .unwrap();
    for name in ["alice", "bob"] {
        let user = UserId::parse(format!("@{name}:preferences.test")).unwrap();
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

async fn request(service: &Service, token: &str, builder: RequestBuilder) -> (StatusCode, Value) {
    let router = service.router();
    let builder = builder.add_header("authorization", format!("Bearer {token}"), true);
    tokio::spawn(async move {
        let mut response = builder.send(router).await;
        (
            response.status_code.unwrap(),
            response.take_json().await.unwrap(),
        )
    })
    .await
    .unwrap()
}

async fn ok(service: &Service, token: &str, builder: RequestBuilder) -> Value {
    let (status, body) = request(service, token, builder).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

async fn classic(service: &Service, since: Option<&str>) -> Value {
    let mut url = url::Url::parse(&format!("{ROOT}/sync?timeout=0")).unwrap();
    url.query_pairs_mut().append_pair(
        "filter",
        &json!({"room":{"timeline":{"limit":0}}}).to_string(),
    );
    if let Some(since) = since {
        url.query_pairs_mut().append_pair("since", since);
    }
    ok(service, "alice", TestClient::get(url.as_str())).await
}

async fn sliding(service: &Service, room: &str, pos: Option<&str>, receipts: bool) -> Value {
    let mut url = url::Url::parse(
        "http://localhost/_matrix/client/unstable/org.matrix.simplified_msc3575/sync?timeout=0",
    )
    .unwrap();
    if let Some(pos) = pos {
        url.query_pairs_mut().append_pair("pos", pos);
    }
    ok(
        service,
        "alice",
        TestClient::post(url.as_str()).json(&json!({
            "conn_id":if receipts { "with-receipts" } else { "without-receipts" },
            "room_subscriptions":{room:{"timeline_limit":0,"required_state":[]}},
            "extensions":{"receipts":{"enabled":receipts},"account_data":{"enabled":true}}
        })),
    )
    .await
}

fn tags(events: &Value) -> &Value {
    &events
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["type"] == "m.tag")
        .unwrap()["content"]["tags"]
}

#[test]
#[ignore = "requires an empty dedicated PALPO_TEST_DATABASE_URL; run alone"]
fn http_room_tags_use_standard_routes_bodies_and_sync_echoes() {
    run(async {
        let service = setup().await;
        let created = ok(
            &service,
            "alice",
            TestClient::post(format!("{ROOT}/createRoom")).json(&json!({})),
        )
        .await;
        let room = created["room_id"].as_str().unwrap();
        let url = format!("{ROOT}/user/{ALICE}/rooms/{room}/tags");
        assert_eq!(
            ok(&service, "alice", TestClient::get(&url)).await,
            json!({"tags":{}})
        );
        let before = classic(&service, None).await;
        let slide = sliding(&service, room, None, false).await;
        // Matrix clients send TagInfo directly, with no `tag_info` wrapper.
        for (name, info) in [
            ("m.favourite", json!({"order":0.25})),
            ("m.lowpriority", json!({})),
        ] {
            ok(
                &service,
                "alice",
                TestClient::put(format!("{url}/{name}")).json(&info),
            )
            .await;
        }
        let expected = json!({"m.favourite":{"order":0.25},"m.lowpriority":{}});
        let legacy = url.replace("/v3/", "/r0/");
        assert_eq!(
            ok(&service, "alice", TestClient::get(&legacy)).await["tags"],
            expected
        );
        let synced = classic(&service, before["next_batch"].as_str()).await;
        assert_eq!(
            tags(&synced["rooms"]["join"][room]["account_data"]["events"]),
            &expected
        );
        let synced = sliding(&service, room, slide["pos"].as_str(), false).await;
        assert_eq!(
            tags(&synced["extensions"]["account_data"]["rooms"][room]),
            &expected
        );
        // Both reads and writes must enforce the user in the URL.
        for builder in [
            TestClient::get(&url),
            TestClient::put(format!("{url}/m.favourite")).json(&json!({})),
            TestClient::delete(format!("{url}/m.favourite")),
        ] {
            let (status, _) = request(&service, "bob", builder).await;
            assert_eq!(status, StatusCode::FORBIDDEN);
        }
        let response = TestClient::get(&url).send(&service).await;
        assert_eq!(response.status_code, Some(StatusCode::UNAUTHORIZED));
        let before = classic(&service, None).await;
        for name in ["m.favourite", "m.lowpriority"] {
            ok(
                &service,
                "alice",
                TestClient::delete(format!("{legacy}/{name}")),
            )
            .await;
        }
        assert_eq!(
            ok(&service, "alice", TestClient::get(&url)).await,
            json!({"tags":{}})
        );
        let synced = classic(&service, before["next_batch"].as_str()).await;
        assert_eq!(
            tags(&synced["rooms"]["join"][room]["account_data"]["events"]),
            &json!({})
        );
    });
}

#[test]
#[ignore = "requires an empty dedicated PALPO_TEST_DATABASE_URL; run alone"]
fn http_reading_old_events_clears_counts_without_new_timeline_events() {
    run(async {
        let service = setup().await;
        let created = ok(
            &service,
            "bob",
            TestClient::post(format!("{ROOT}/createRoom")).json(&json!({"invite":[ALICE]})),
        )
        .await;
        let room = created["room_id"].as_str().unwrap();
        ok(
            &service,
            "alice",
            TestClient::post(format!("{ROOT}/join/{room}")).json(&json!({})),
        )
        .await;
        for (i, kind) in ["m.read", "m.read.private"].into_iter().enumerate() {
            let sent = ok(
                &service,
                "bob",
                TestClient::put(format!("{ROOT}/rooms/{room}/send/m.room.message/m{i}"))
                    .json(&json!({"msgtype":"m.text","body":"unread fixture"})),
            )
            .await;
            let before = classic(&service, None).await;
            // The initial invitation can also have a pending notification.
            assert!(
                before["rooms"]["join"][room]["unread_notifications"]["notification_count"]
                    .as_u64()
                    .unwrap()
                    > 0
            );
            let slides = [
                sliding(&service, room, None, false).await,
                sliding(&service, room, None, true).await,
            ];
            ok(
                &service,
                "alice",
                TestClient::post(format!(
                    "{ROOT}/rooms/{room}/receipt/{kind}/{}",
                    sent["event_id"].as_str().unwrap()
                ))
                .json(&json!({})),
            )
            .await;
            let synced = classic(&service, before["next_batch"].as_str()).await;
            assert_eq!(
                synced["rooms"]["join"][room]["unread_notifications"]["notification_count"], 0,
                "{synced}"
            );
            assert!(
                synced["rooms"]["join"][room]["timeline"]["events"]
                    .as_array()
                    .is_none_or(Vec::is_empty)
            );
            for (receipts, slide) in [false, true].into_iter().zip(slides) {
                let synced = sliding(&service, room, slide["pos"].as_str(), receipts).await;
                assert_eq!(synced["rooms"][room]["notification_count"], 0, "{synced}");
                let idle = sliding(&service, room, synced["pos"].as_str(), receipts).await;
                assert!(idle["rooms"].get(room).is_none(), "{idle}");
            }
        }
    });
}
