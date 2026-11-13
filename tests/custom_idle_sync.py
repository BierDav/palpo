#!/usr/bin/env python3
"""Exercise the built binary against an isolated PostgreSQL, never production."""
import json
import subprocess
import shlex
import sys
import time
import uuid


def docker(*args, data=None):
    return subprocess.check_output(["docker", *args], input=data, text=True).strip()


image = sys.argv[1]
name = "palpo-key-count-test-" + uuid.uuid4().hex[:10]
database, server = name + "-db", name + "-server"
try:
    docker("network", "create", name)
    docker("run", "-d", "--name", database, "--network", name,
           "-e", "POSTGRES_PASSWORD=fixture", "-e", "POSTGRES_DB=palpo", "postgres:18-bookworm")
    for _ in range(60):
        if subprocess.run(["docker", "exec", database, "pg_isready", "-U", "postgres"],
                          stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode == 0:
            break
        time.sleep(1)
    else:
        raise RuntimeError("fixture database did not start")
    config = f'''server_name = "fixture.test"
allow_registration = false
[[listeners]]
address = "0.0.0.0:8008"
[db]
url = "postgresql://postgres:fixture@{database}:5432/palpo"
[federation]
enable = false
'''
    docker("run", "-d", "--name", server, "--network", name,
           "-e", "PALPO_CONFIG=/var/palpo/palpo.toml", "--entrypoint", "/bin/sh", image,
           "-ec", "printf %s " + shlex.quote(config) + " > /var/palpo/palpo.toml; exec /var/palpo/palpo")
    for _ in range(90):
        result = subprocess.run(["docker", "exec", server, "curl", "-fsS", "http://localhost:8008/healthz"],
                                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        if result.returncode == 0:
            break
        time.sleep(1)
    else:
        raise RuntimeError("fixture server did not start")
    docker("exec", "-i", database, "psql", "-v", "ON_ERROR_STOP=1", "-U", "postgres", "-d", "palpo", data='''
INSERT INTO users (id, localpart, server_name, created_at)
VALUES ('@fixture:fixture.test', 'fixture', 'fixture.test', 0),
       ('@other:fixture.test', 'other', 'fixture.test', 0);
INSERT INTO user_devices (user_id, device_id, created_at)
VALUES ('@fixture:fixture.test', 'FIXTURE', 0),
       ('@other:fixture.test', 'OTHER', 0);
INSERT INTO user_access_tokens (user_id, device_id, token, created_at)
VALUES ('@fixture:fixture.test', 'FIXTURE', 'fixture-access', 0),
       ('@other:fixture.test', 'OTHER', 'other-access', 0);
''')

    def request(path, body=None, method=None, token="fixture-access"):
        args = ["exec", server, "curl", "-fsS", "--max-time", "15",
                "-H", f"Authorization: Bearer {token}"]
        if method is not None:
            args += ["-X", method]
        if body is not None:
            args += ["-H", "Content-Type: application/json", "--data", json.dumps(body)]
        return json.loads(docker(*args, "http://localhost:8008/_matrix/client/v3/" + path))

    uploaded = request("keys/upload", {"one_time_keys": {"signed_curve25519:once": "b25jZQ"}})
    assert uploaded["one_time_key_counts"]["signed_curve25519"] == 1, uploaded
    initial = request("sync?timeout=0")
    token = initial["next_batch"]
    for _ in range(3):
        idle = request("sync?timeout=0&since=" + token)
        assert idle["device_one_time_keys_count"]["signed_curve25519"] == 1, idle
        token = idle["next_batch"]
    request("keys/claim", {"one_time_keys": {"@fixture:fixture.test": {"FIXTURE": "signed_curve25519"}}})
    idle = request("sync?timeout=0&since=" + token)
    assert idle.get("device_one_time_keys_count", {}).get("signed_curve25519", 0) == 0, idle

    # Verify active_joined_rooms optimization delivers both to_device events and room messages.
    import urllib.parse
    rooms = [request("createRoom", {})["room_id"] for _ in range(15)]
    other_room = request("createRoom", {}, token="other-access")["room_id"]
    token = request("sync?timeout=0")["next_batch"]
    # Unrelated room traffic advances curr_sn past token; to_device still arrives fast.
    request(f"rooms/{urllib.parse.quote(other_room, safe='')}/send/m.room.message/t0",
            {"msgtype": "m.text", "body": "unrelated"}, method="PUT", token="other-access")
    request("sendToDevice/m.room.encrypted/td0",
            {"messages": {"@fixture:fixture.test": {"FIXTURE": {"hello": "world"}}}}, method="PUT")
    bridge_filter = urllib.parse.quote(json.dumps({
        "account_data": {"not_types": ["*"]},
        "presence": {"not_types": ["*"]},
        "room": {
            "account_data": {"not_types": ["*"]},
            "ephemeral": {"not_types": ["*"]},
            "state": {"not_types": ["*"]},
            "timeline": {"not_types": ["*"]},
        },
    }), safe="")
    td_sync = request(f"sync?timeout=0&since={token}&filter={bridge_filter}")
    assert len(td_sync.get("to_device", {}).get("events", [])) == 1, td_sync
    assert td_sync.get("rooms", {}).get("join", {}) == {}, td_sync
    token = td_sync["next_batch"]

    target_room = rooms[7]
    sent = request(f"rooms/{urllib.parse.quote(target_room, safe='')}/send/m.room.message/t1",
                   {"msgtype": "m.text", "body": "ping"}, method="PUT")
    room_sync = request(f"sync?timeout=0&since={token}")
    joined = room_sync.get("rooms", {}).get("join", {})
    assert target_room in joined, room_sync
    event_ids = [e["event_id"] for e in joined[target_room]["timeline"]["events"]]
    assert sent["event_id"] in event_ids, joined[target_room]
    print("PASS: repeated idle syncs report current key counts and active-room incremental sync delivers to_device and room events")
except Exception:
    subprocess.run(["docker", "logs", server, "--tail", "60"])
    raise
finally:
    subprocess.run(["docker", "rm", "-f", "-v", server, database], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    subprocess.run(["docker", "network", "rm", name], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
