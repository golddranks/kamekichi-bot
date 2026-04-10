use super::msg::Event;
use super::*;

fn ev(event_type: &str, data: serde_json::Value) -> Result<Option<Event>> {
    parse_event(EventType::from_name(event_type), &data.to_string())
}

#[test]
fn parse_message_create() {
    let data = serde_json::json!({
        "id": "123",
        "channel_id": "456",
        "author": { "id": "789", "username": "testuser" },
        "content": "hello world"
    });
    match ev("MESSAGE_CREATE", data) {
        Ok(Some(Event::MessageCreate(m))) => {
            assert_eq!(m.id, 123);
            assert_eq!(m.channel_id, 456);
            assert_eq!(m.author.id, 789);
            assert_eq!(m.author.username, "testuser");
            assert_eq!(m.content, "hello world");
        }
        other => panic!("expected MessageCreate, got {other:?}"),
    }
}

#[test]
fn parse_message_update_with_content() {
    let data = serde_json::json!({
        "id": "100",
        "channel_id": "200",
        "content": "edited"
    });
    match ev("MESSAGE_UPDATE", data) {
        Ok(Some(Event::MessageUpdate(m))) => {
            assert_eq!(m.id, 100);
            assert_eq!(m.channel_id, 200);
            assert_eq!(m.content, Some("edited".into()));
        }
        other => panic!("expected MessageUpdate, got {other:?}"),
    }
}

#[test]
fn parse_message_update_without_content() {
    let data = serde_json::json!({
        "id": "100",
        "channel_id": "200"
    });
    match ev("MESSAGE_UPDATE", data) {
        Ok(Some(Event::MessageUpdate(m))) => {
            assert_eq!(m.content, None);
        }
        other => panic!("expected MessageUpdate, got {other:?}"),
    }
}

#[test]
fn parse_message_delete() {
    let data = serde_json::json!({
        "id": "100",
        "channel_id": "200"
    });
    match ev("MESSAGE_DELETE", data) {
        Ok(Some(Event::MessageDelete(m))) => {
            assert_eq!(m.id, 100);
            assert_eq!(m.channel_id, 200);
        }
        other => panic!("expected MessageDelete, got {other:?}"),
    }
}

#[test]
fn parse_reaction_add_unicode() {
    let data = serde_json::json!({
        "user_id": "1",
        "channel_id": "2",
        "message_id": "3",
        "emoji": { "name": "\u{1f44d}", "id": null }
    });
    match ev("MESSAGE_REACTION_ADD", data) {
        Ok(Some(Event::ReactionAdd(r))) => {
            assert_eq!(r.emoji.to_string(), "\u{1f44d}");
        }
        other => panic!("expected ReactionAdd, got {other:?}"),
    }
}

#[test]
fn parse_reaction_remove_custom() {
    let data = serde_json::json!({
        "user_id": "1",
        "channel_id": "2",
        "message_id": "3",
        "emoji": { "name": "kame", "id": "999" }
    });
    match ev("MESSAGE_REACTION_REMOVE", data) {
        Ok(Some(Event::ReactionRemove(r))) => {
            assert_eq!(r.emoji.to_string(), "kame:999");
        }
        other => panic!("expected ReactionRemove, got {other:?}"),
    }
}

#[test]
fn parse_unknown_event() {
    let data = serde_json::json!({});
    assert!(matches!(ev("TYPING_START", data), Ok(None)));
}

#[test]
fn parse_message_create_missing_author() {
    let data = serde_json::json!({
        "id": "123",
        "channel_id": "456",
        "content": "hello"
    });
    assert!(ev("MESSAGE_CREATE", data).is_err());
}

#[test]
fn parse_message_create_null_content() {
    let data = serde_json::json!({
        "id": "123",
        "channel_id": "456",
        "author": { "id": "789", "username": "bot" },
        "content": null
    });
    match ev("MESSAGE_CREATE", data) {
        Ok(Some(Event::MessageCreate(m))) => {
            assert_eq!(m.content, "");
        }
        other => panic!("expected MessageCreate, got {other:?}"),
    }
}

#[test]
fn parse_message_create_missing_content() {
    let data = serde_json::json!({
        "id": "123",
        "channel_id": "456",
        "author": { "id": "789", "username": "bot" }
    });
    match ev("MESSAGE_CREATE", data) {
        Ok(Some(Event::MessageCreate(m))) => {
            assert_eq!(m.content, "");
        }
        other => panic!("expected MessageCreate, got {other:?}"),
    }
}

#[test]
fn parse_message_create_missing_username() {
    let data = serde_json::json!({
        "id": "123",
        "channel_id": "456",
        "author": { "id": "789" },
        "content": "hello"
    });
    match ev("MESSAGE_CREATE", data) {
        Ok(Some(Event::MessageCreate(m))) => {
            assert_eq!(m.author.username, "");
            assert_eq!(m.content, "hello");
        }
        other => panic!("expected MessageCreate, got {other:?}"),
    }
}

#[test]
fn parse_ready() {
    let data = serde_json::json!({
        "user": { "id": "111" },
        "session_id": "abc",
        "resume_gateway_url": "wss://gateway-us-east1-b.discord.gg",
        "guilds": [{ "id": "100" }, { "id": "200" }]
    });
    match ev("READY", data) {
        Ok(Some(Event::Ready(r))) => {
            assert_eq!(r.session_id, "abc");
            assert_eq!(r.resume_gateway_url, "wss://gateway-us-east1-b.discord.gg");
            assert_eq!(r.user.id, 111);
            let ids: Vec<u64> = r.guilds.iter().map(|g| g.id).collect();
            assert_eq!(ids, vec![100, 200]);
        }
        other => panic!("expected Ready, got {other:?}"),
    }
}

#[test]
fn parse_ready_missing_session_id() {
    let data = serde_json::json!({
        "user": { "id": "111" },
        "resume_gateway_url": "wss://gw.discord.gg",
        "guilds": []
    });
    assert!(ev("READY", data).is_err());
}

#[test]
fn parse_ready_missing_user() {
    let data = serde_json::json!({
        "session_id": "abc",
        "resume_gateway_url": "wss://gw.discord.gg",
        "guilds": []
    });
    assert!(ev("READY", data).is_err());
}

#[test]
fn parse_ready_missing_resume_url() {
    let data = serde_json::json!({
        "user": { "id": "111" },
        "session_id": "abc",
        "guilds": []
    });
    assert!(ev("READY", data).is_err());
}

#[test]
fn parse_resumed() {
    let data = serde_json::json!({});
    assert!(matches!(ev("RESUMED", data), Ok(Some(Event::Resumed))));
}

#[test]
fn parse_guild_create() {
    let data = serde_json::json!({
        "id": "100",
        "channels": [
            { "id": "1", "name": "general" },
            { "id": "2", "name": "bot" }
        ],
        "roles": [
            { "id": "10", "name": "admin" }
        ]
    });
    match ev("GUILD_CREATE", data) {
        Ok(Some(Event::GuildCreate(gc))) => {
            assert_eq!(gc.id, 100);
            let channels: Vec<(u64, &str)> = gc
                .channels
                .iter()
                .map(|c| (c.id, c.name.as_str()))
                .collect();
            assert_eq!(channels, vec![(1, "general"), (2, "bot")]);
            let roles: Vec<(u64, &str)> =
                gc.roles.iter().map(|r| (r.id, r.name.as_str())).collect();
            assert_eq!(roles, vec![(10, "admin")]);
        }
        other => panic!("expected GuildCreate, got {other:?}"),
    }
}

#[test]
fn parse_known_event_bad_data_returns_error() {
    let data = serde_json::json!({ "not": "a valid message" });
    let err = ev("MESSAGE_CREATE", data).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("missing field"),
        "expected missing-field error, got: {msg}"
    );
}

#[test]
fn parse_reaction_bad_data_returns_error() {
    let data = serde_json::json!({ "user_id": "1" });
    assert!(ev("MESSAGE_REACTION_ADD", data).is_err());
}

#[test]
fn parse_guild_create_bad_snowflake_returns_error() {
    let data = serde_json::json!({ "id": "not_a_number" });
    assert!(ev("GUILD_CREATE", data).is_err());
}

// ---- process_text ----

fn gw(op: u8, d: Option<serde_json::Value>, s: Option<u64>, t: Option<&str>) -> String {
    serde_json::json!({ "op": op, "d": d, "s": s, "t": t }).to_string()
}

#[test]
fn process_heartbeat_ack() {
    assert!(matches!(
        process_text(&gw(OP_HEARTBEAT_ACK, None, None, None)),
        Ok(GatewayAction::HeartbeatAck)
    ));
}

#[test]
fn process_server_heartbeat() {
    assert!(matches!(
        process_text(&gw(OP_HEARTBEAT, None, None, None)),
        Ok(GatewayAction::SendHeartbeat)
    ));
}

#[test]
fn process_reconnect() {
    assert!(matches!(
        process_text(&gw(OP_RECONNECT, None, None, None)),
        Err(Error::Reconnect)
    ));
}

#[test]
fn process_invalid_session_resumable() {
    let text = gw(
        OP_INVALID_SESSION,
        Some(serde_json::json!(true)),
        None,
        None,
    );
    assert!(matches!(
        process_text(&text),
        Err(Error::InvalidSession { resumable: true })
    ));
}

#[test]
fn process_invalid_session_not_resumable() {
    let text = gw(
        OP_INVALID_SESSION,
        Some(serde_json::json!(false)),
        None,
        None,
    );
    assert!(matches!(
        process_text(&text),
        Err(Error::InvalidSession { resumable: false })
    ));
}

#[test]
fn process_invalid_session_null_data() {
    let text = gw(OP_INVALID_SESSION, None, None, None);
    assert!(matches!(
        process_text(&text),
        Err(Error::InvalidSession { resumable: false })
    ));
}

#[test]
fn process_dispatch() {
    let text = gw(
        OP_DISPATCH,
        Some(serde_json::json!({"content": "hi"})),
        Some(42),
        Some("MESSAGE_CREATE"),
    );
    match process_text(&text).unwrap() {
        GatewayAction::Dispatch(d) => {
            assert_eq!(d.sequence, 42);
            assert_eq!(d.event_type, Some(EventType::MessageCreate));
        }
        other => panic!("expected Dispatch, got {other:?}"),
    }
}

#[test]
fn process_dispatch_missing_sequence() {
    let text = gw(
        OP_DISPATCH,
        Some(serde_json::json!({})),
        None,
        Some("READY"),
    );
    assert!(matches!(process_text(&text), Err(Error::MalformedDispatch)));
}

#[test]
fn process_dispatch_missing_event_type() {
    let text = gw(OP_DISPATCH, Some(serde_json::json!({})), Some(1), None);
    match process_text(&text).unwrap() {
        GatewayAction::Dispatch(d) => {
            assert_eq!(d.sequence, 1);
            assert!(d.event_type.is_none());
            assert!(d.data.is_some());
        }
        other => panic!("expected Dispatch, got {other:?}"),
    }
}

#[test]
fn process_dispatch_missing_data() {
    let text = gw(OP_DISPATCH, None, Some(1), Some("READY"));
    match process_text(&text).unwrap() {
        GatewayAction::Dispatch(d) => {
            assert_eq!(d.sequence, 1);
            assert_eq!(d.event_type, Some(EventType::Ready));
            assert!(d.data.is_none());
        }
        other => panic!("expected Dispatch, got {other:?}"),
    }
}

#[test]
fn process_unknown_opcode() {
    let text = gw(99, None, None, None);
    assert!(matches!(
        process_text(&text),
        Err(Error::UnexpectedOpcode(99))
    ));
}

#[test]
fn process_malformed_json() {
    assert!(matches!(process_text("not json"), Err(Error::Json(_))));
}

// ---- parse_gateway_url ----

#[test]
fn gateway_url_host_only() {
    let u = parse_gateway_url("wss://gateway-us-east1-b.discord.gg").unwrap();
    assert_eq!(u.host, "gateway-us-east1-b.discord.gg");
    assert_eq!(u.port, 443);
    assert_eq!(u.path, "/");
}

#[test]
fn gateway_url_with_path() {
    let u = parse_gateway_url("wss://gw.discord.gg/?v=10&encoding=json").unwrap();
    assert_eq!(u.host, "gw.discord.gg");
    assert_eq!(u.port, 443);
    assert_eq!(u.path, "/?v=10&encoding=json");
}

#[test]
fn gateway_url_with_port() {
    let u = parse_gateway_url("wss://gw.discord.gg:8443/ws").unwrap();
    assert_eq!(u.host, "gw.discord.gg");
    assert_eq!(u.port, 8443);
    assert_eq!(u.path, "/ws");
}

#[test]
fn gateway_url_with_port_no_path() {
    let u = parse_gateway_url("wss://gw.discord.gg:443").unwrap();
    assert_eq!(u.host, "gw.discord.gg");
    assert_eq!(u.port, 443);
    assert_eq!(u.path, "/");
}

#[test]
fn gateway_url_query_no_slash() {
    assert!(parse_gateway_url("wss://gw.discord.gg?v=10").is_err());
}

#[test]
fn gateway_url_bad_scheme() {
    assert!(parse_gateway_url("https://gw.discord.gg").is_err());
}

#[test]
fn gateway_url_bad_port() {
    assert!(parse_gateway_url("wss://gw.discord.gg:notaport/path").is_err());
}

#[test]
fn gateway_url_empty_host() {
    assert!(parse_gateway_url("wss://").is_err());
    assert!(parse_gateway_url("wss://:443/path").is_err());
}

#[test]
fn gateway_url_ipv6_rejected() {
    assert!(parse_gateway_url("wss://[::1]:443/path").is_err());
    assert!(parse_gateway_url("wss://[::1]/path").is_err());
}

// ---- is_safe_for_json_literal ----

#[test]
fn json_literal_safe_token() {
    assert!(is_safe_for_json_literal(
        "Bot MTIzNDU2Nzg5.abc123.xyz_ABC-789"
    ));
}

#[test]
fn json_literal_rejects_quote() {
    assert!(is_safe_for_json_literal(r#"has"quote"#).not());
}

#[test]
fn json_literal_rejects_backslash() {
    assert!(is_safe_for_json_literal(r"has\slash").not());
}

#[test]
fn json_literal_rejects_non_ascii() {
    assert!(is_safe_for_json_literal("caf\u{e9}").not());
}

#[test]
fn json_literal_rejects_control_chars() {
    assert!(is_safe_for_json_literal("has\nnewline").not());
    assert!(is_safe_for_json_literal("has\0null").not());
    assert!(is_safe_for_json_literal("has\ttab").not());
}

#[test]
fn json_literal_accepts_empty() {
    assert!(is_safe_for_json_literal(""));
}

#[test]
fn json_literal_accepts_all_safe_ascii() {
    let safe: String = (b' '..=b'~')
        .filter(|&b| b != b'"' && b != b'\\')
        .map(|b| b as char)
        .collect();
    assert!(is_safe_for_json_literal(&safe));
}

#[test]
fn json_literal_os_constant_is_safe() {
    assert!(
        is_safe_for_json_literal(std::env::consts::OS),
        "std::env::consts::OS ({:?}) must be safe for JSON literal interpolation",
        std::env::consts::OS,
    );
}

// ---- classify_error ----

#[test]
fn classify_fatal_close_codes() {
    for code in [4004, 4010, 4011, 4012, 4013, 4014] {
        assert_eq!(
            classify_error(&Error::GatewayClosed {
                code: Some(code),
                reason: String::new()
            }),
            ErrorClass::Fatal,
            "code {code}"
        );
    }
}

#[test]
fn classify_non_resumable_close_codes() {
    for code in [4007, 4009] {
        assert_eq!(
            classify_error(&Error::GatewayClosed {
                code: Some(code),
                reason: String::new()
            }),
            ErrorClass::Reconnect,
            "code {code}"
        );
    }
}

#[test]
fn classify_other_close_codes_resume() {
    for code in [4000, 4001, 4002, 4003, 4005, 4008] {
        assert_eq!(
            classify_error(&Error::GatewayClosed {
                code: Some(code),
                reason: String::new()
            }),
            ErrorClass::Resume,
            "code {code}"
        );
    }
}

#[test]
fn classify_connection_errors_resume() {
    assert_eq!(classify_error(&Error::Reconnect), ErrorClass::Resume);
    assert_eq!(
        classify_error(&Error::ConnectionZombied),
        ErrorClass::Resume
    );
    assert_eq!(classify_error(&Error::ConnectionClosed), ErrorClass::Resume);
    assert_eq!(
        classify_error(&Error::Io(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            ""
        ))),
        ErrorClass::Resume
    );
    assert_eq!(
        classify_error(&Error::WebSocket(kamekichi_ws::Error::Reconnect(
            kamekichi_ws::ConnectionError::Closed
        ))),
        ErrorClass::Resume
    );
    assert_eq!(
        classify_error(&Error::InvalidSession { resumable: true }),
        ErrorClass::Resume
    );
}

#[test]
fn classify_invalid_session_not_resumable() {
    assert_eq!(
        classify_error(&Error::InvalidSession { resumable: false }),
        ErrorClass::Reconnect
    );
}

#[test]
fn classify_transient_errors() {
    assert_eq!(
        classify_error(&Error::Json(serde_json::from_str::<()>("x").unwrap_err())),
        ErrorClass::Transient
    );
    assert_eq!(
        classify_error(&Error::UnexpectedOpcode(99)),
        ErrorClass::Transient
    );
    assert_eq!(
        classify_error(&Error::UnexpectedBinary),
        ErrorClass::Transient
    );
    assert_eq!(
        classify_error(&Error::MalformedDispatch),
        ErrorClass::Transient
    );
    assert_eq!(
        classify_error(&Error::MissingPayload),
        ErrorClass::Transient
    );
}
