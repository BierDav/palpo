//! Device-list windows across many rooms: exact `changed`/`left` results for
//! joins, leaves, key uploads and new devices, with bounded SQL per window.
use salvo::prelude::*;
use salvo::test::{RequestBuilder, ResponseExt, TestClient};
use serde_json::{Value, json};

use crate::core::{OwnedDeviceId, OwnedRoomId, OwnedUserId, UnixMillis, UserId};
use crate::{config, data};

/// Plaintext rooms the requester shares with nobody. A per-room walk would
/// issue at least two statements for each of them on every window.
const FILLER_ROOMS: usize = 40;

#[test]
#[ignore = "requires an empty dedicated PALPO_TEST_DATABASE_URL; run alone"]
fn review_device_lists_batch_rooms_and_pin_changed_and_left() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(run())
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn ok(request: RequestBuilder, service: &Service) -> Value {
    let router = service.router();
    let mut response = tokio::spawn(async move { request.send(router).await })
        .await
        .unwrap();
    let body: Value = response.take_json().await.unwrap();
    assert_eq!(response.status_code, Some(StatusCode::OK), "{body}");
    body
}

async fn provision(localpart: &str) -> (OwnedUserId, String) {
    let user = UserId::parse(format!("@{localpart}:{}", config::server_name())).unwrap();
    data::user::create_user(&data::user::NewDbUser {
        id: user.clone(),
        ty: None,
        is_admin: false,
        is_guest: false,
        is_local: true,
        localpart: localpart.into(),
        server_name: user.server_name().to_owned(),
        appservice_id: None,
        created_at: UnixMillis::now(),
    })
    .await
    .unwrap();
    data::user::ensure_profile_exists(&user).await.unwrap();
    let token = format!("{localpart}-token");
    data::user::device::create_device(&user, &OwnedDeviceId::from("D"), &token, None, None)
        .await
        .unwrap();
    crate::user::get_push_rules(&user).await.unwrap();
    (user, token)
}

async fn create(service: &Service, token: &str, encrypted: bool) -> OwnedRoomId {
    let initial = if encrypted {
        json!([{"type":"m.room.encryption","state_key":"","content":{"algorithm":"m.megolm.v1.aes-sha2"}}])
    } else {
        json!([])
    };
    let body = ok(
        TestClient::post("http://localhost/_matrix/client/v3/createRoom")
            .add_header("Authorization", format!("Bearer {token}"), true)
            .json(&json!({"room_version":"11","preset":"public_chat","initial_state":initial})),
        service,
    )
    .await;
    body["room_id"].as_str().unwrap().try_into().unwrap()
}

async fn membership(service: &Service, room: &OwnedRoomId, token: &str, action: &str) {
    ok(
        TestClient::post(format!(
            "http://localhost/_matrix/client/v3/rooms/{room}/{action}"
        ))
        .add_header("Authorization", format!("Bearer {token}"), true)
        .json(&json!({})),
        service,
    )
    .await;
}

/// Publish device keys, as a client does for a new device or a key rotation.
async fn upload_keys(service: &Service, user: &UserId, device: &str, token: &str, key: &str) {
    ok(
        TestClient::post("http://localhost/_matrix/client/v3/keys/upload")
            .add_header("Authorization", format!("Bearer {token}"), true)
            .json(&json!({"device_keys":{
                "user_id": user,
                "device_id": device,
                "algorithms": ["m.olm.v1.curve25519-aes-sha2", "m.megolm.v1.aes-sha2"],
                "keys": {
                    format!("curve25519:{device}"): key,
                    format!("ed25519:{device}"): key,
                },
                "signatures": {},
            }})),
        service,
    )
    .await;
}

struct Requester<'a> {
    service: &'a Service,
    user: &'a OwnedUserId,
    token: &'a str,
    since: String,
}

impl Requester<'_> {
    /// Sync one window and pin its device lists, which /keys/changes must
    /// repeat exactly for the same tokens.
    async fn window(&mut self, changed: &[&OwnedUserId], left: &[&OwnedUserId], case: &str) {
        let synced = ok(
            TestClient::get(format!(
                "http://localhost/_matrix/client/v3/sync?timeout=0&since={}",
                self.since
            ))
            .add_header("Authorization", format!("Bearer {}", self.token), true),
            self.service,
        )
        .await;
        let next = synced["next_batch"].as_str().unwrap().to_owned();
        let expected = json!({"changed": changed, "left": left});
        let lists = synced
            .get("device_lists")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let sync_lists = json!({
            "changed": lists.get("changed").cloned().unwrap_or_else(|| json!([])),
            "left": lists.get("left").cloned().unwrap_or_else(|| json!([])),
        });
        assert_eq!(sync_lists, expected, "sync: {case}");
        let key_changes = ok(
            TestClient::get(format!(
                "http://localhost/_matrix/client/v3/keys/changes?from={}&to={next}",
                self.since
            ))
            .add_header("Authorization", format!("Bearer {}", self.token), true),
            self.service,
        )
        .await;
        assert_eq!(key_changes, expected, "keys/changes: {case}");
        self.since = next;
    }

    /// Recompute an already synced window from immutable endpoints and count
    /// the statements. The result must not depend on the frame cache.
    async fn statements(&self, from: &str, to: &str, expected: &Value, case: &str) -> usize {
        let from = from.parse::<crate::event::BatchToken>().unwrap().event_sn();
        let to = to.parse::<crate::event::BatchToken>().unwrap().event_sn();
        let spy = crate::test_database::sql_spy::SqlSpy::install().await;
        let lists = super::changes(self.user, from, to).await.unwrap();
        let queries = spy.snapshot();
        spy.assert_coverage().await;
        spy.uninstall().await;
        assert_eq!(
            json!({"changed": lists.changed, "left": lists.left}),
            *expected,
            "recomputed: {case}"
        );
        // Pool checkouts validate connections with a constant probe.
        queries
            .iter()
            .filter(|query| !query.starts_with("SELECT $1 -- binds: [1]"))
            .count()
    }
}

async fn run() {
    crate::test_database::init();
    config::CONFIG.set(serde_json::from_value(json!({"server_name":"device-batch.example","db":{"url":"unused-test-config"},"ip_range_denylist":[],"auto_join_rooms":[]})).unwrap()).unwrap();
    let (alice, at) = provision("alice").await;
    let (bob, bt) = provision("bob").await;
    let (carol, ct) = provision("carol").await;
    let (dave, dt) = provision("dave").await;
    let (erin, et) = provision("erin").await;
    let service = Service::new(crate::routing::root());

    let encrypted_one = create(&service, &at, true).await;
    let encrypted_two = create(&service, &at, true).await;
    let plain_one = create(&service, &at, false).await;
    let plain_two = create(&service, &at, false).await;
    for _ in 0..FILLER_ROOMS {
        create(&service, &at, false).await;
    }
    let initial = ok(
        TestClient::get("http://localhost/_matrix/client/v3/sync?timeout=0").add_header(
            "Authorization",
            format!("Bearer {at}"),
            true,
        ),
        &service,
    )
    .await;
    let start = initial["next_batch"].as_str().unwrap().to_owned();
    let mut alice_sync = Requester {
        service: &service,
        user: &alice,
        token: &at,
        since: start.clone(),
    };

    // Joins in several rooms, encrypted and plaintext.
    membership(&service, &encrypted_one, &bt, "join").await;
    membership(&service, &plain_one, &ct, "join").await;
    membership(&service, &encrypted_two, &dt, "join").await;
    let joins_from = alice_sync.since.clone();
    alice_sync
        .window(&[&bob, &carol, &dave], &[], "peers join three rooms")
        .await;
    let joins_to = alice_sync.since.clone();

    // A new device appears when it publishes its keys.
    data::user::device::create_device(&bob, &OwnedDeviceId::from("B2"), "bob-second", None, None)
        .await
        .unwrap();
    upload_keys(&service, &bob, "B2", "bob-second", "bob-second-key").await;
    alice_sync
        .window(&[&bob], &[], "encrypted peer adds a device")
        .await;

    // Key uploads by a plaintext peer are reported; a stranger's are not.
    upload_keys(&service, &carol, "D", &ct, "carol-key").await;
    upload_keys(&service, &erin, "D", &et, "erin-key").await;
    let keys_from = alice_sync.since.clone();
    alice_sync
        .window(&[&carol], &[], "plaintext peer uploads keys")
        .await;
    let keys_to = alice_sync.since.clone();

    // Membership continuity proves nothing new when an already shared peer
    // joins a second room, but a local /join publishes the joiner's own
    // device-list marker, so the peer is still reported once.
    membership(&service, &plain_two, &bt, "join").await;
    alice_sync
        .window(&[&bob], &[], "peer joins a second shared room")
        .await;

    // Leaving the last encrypted room ends tracking even though a plaintext
    // room is still shared; leaving the last shared room ends it too.
    membership(&service, &encrypted_one, &bt, "leave").await;
    membership(&service, &encrypted_two, &dt, "leave").await;
    alice_sync
        .window(&[], &[&bob, &dave], "peers leave encrypted rooms")
        .await;

    // A join and a key upload in the same window report the peer once.
    membership(&service, &encrypted_one, &et, "join").await;
    upload_keys(&service, &erin, "D", &et, "erin-rotated").await;
    upload_keys(&service, &carol, "D", &ct, "carol-rotated").await;
    alice_sync
        .window(&[&carol, &erin], &[], "join with key upload")
        .await;

    // The requester's own join notifies its other devices, but not about a
    // peer it already shares a continuous room with.
    let erin_room = create(&service, &et, true).await;
    membership(&service, &erin_room, &at, "join").await;
    alice_sync
        .window(&[&alice], &[], "requester joins a peer's room")
        .await;

    // The requester's own leave ends sharing with that room's only peer.
    membership(&service, &plain_one, &at, "leave").await;
    alice_sync
        .window(&[], &[&carol], "requester leaves a shared room")
        .await;

    // Own key uploads are always reported.
    data::user::device::create_device(
        &alice,
        &OwnedDeviceId::from("A2"),
        "alice-second",
        None,
        None,
    )
    .await
    .unwrap();
    upload_keys(&service, &alice, "A2", "alice-second", "alice-second-key").await;
    alice_sync
        .window(&[&alice], &[], "requester adds a device")
        .await;
    alice_sync.window(&[], &[], "idle").await;

    // Historical windows stay frozen. The whole range nets out the leaves,
    // and carol's key uploads drop out with the last room shared with her.
    let whole = json!({"changed": [&alice, &bob, &erin], "left": []});
    let idle_to = alice_sync.since.clone();
    alice_sync.since = start.clone();
    alice_sync
        .window(&[&alice, &bob, &erin], &[], "whole history")
        .await;

    // Statements per window are bounded independently of the 45 rooms the
    // requester was ever in. Cold frames are decoded once; repeat windows
    // reuse them.
    let rooms = FILLER_ROOMS + 5;
    let membership_cold = alice_sync
        .statements(
            &joins_from,
            &joins_to,
            &json!({"changed": [&bob, &carol, &dave], "left": []}),
            "joins",
        )
        .await;
    let membership_warm = alice_sync
        .statements(
            &joins_from,
            &joins_to,
            &json!({"changed": [&bob, &carol, &dave], "left": []}),
            "joins again",
        )
        .await;
    assert!(
        membership_warm <= 2,
        "a repeated membership window issued {membership_warm} statements (cold {membership_cold}) for {rooms} rooms"
    );
    let keys = alice_sync
        .statements(
            &keys_from,
            &keys_to,
            &json!({"changed": [&carol], "left": []}),
            "key upload",
        )
        .await;
    assert!(
        keys <= 2,
        "a key-only window issued {keys} statements for {rooms} rooms"
    );
    let idle = alice_sync
        .statements(
            &idle_to,
            &idle_to,
            &json!({"changed": [], "left": []}),
            "empty",
        )
        .await;
    assert_eq!(idle, 0, "an empty window must not touch the database");
    let history = alice_sync
        .statements(&start, &idle_to, &whole, "whole")
        .await;
    assert!(
        history < rooms,
        "a historical window issued {history} statements for {rooms} rooms"
    );
}
