use bytes::{BufMut, Bytes, BytesMut};
use gritty::protocol::{Frame, FrameCodec, SessionEntry};
use tokio_util::codec::{Decoder, Encoder};

#[test]
fn encode_data_frame() {
    let mut codec = FrameCodec;
    let mut buf = BytesMut::new();
    codec.encode(Frame::Data(Bytes::from("hello")), &mut buf).unwrap();
    // type(1) + len(4) + payload(5) = 10
    assert_eq!(buf.len(), 10);
    assert_eq!(buf[0], 0x01);
    assert_eq!(&buf[5..], b"hello");
}

#[test]
fn encode_resize_frame() {
    let mut codec = FrameCodec;
    let mut buf = BytesMut::new();
    codec.encode(Frame::Resize { cols: 80, rows: 24 }, &mut buf).unwrap();
    // type(1) + len(4) + payload(4) = 9
    assert_eq!(buf.len(), 9);
    assert_eq!(buf[0], 0x02);
}

#[test]
fn encode_exit_frame() {
    let mut codec = FrameCodec;
    let mut buf = BytesMut::new();
    codec.encode(Frame::Exit { code: 42 }, &mut buf).unwrap();
    assert_eq!(buf.len(), 9);
    assert_eq!(buf[0], 0x03);
}

#[test]
fn roundtrip_data() {
    let mut codec = FrameCodec;
    let mut buf = BytesMut::new();
    let original = Frame::Data(Bytes::from("hello world"));
    codec.encode(original.clone(), &mut buf).unwrap();
    let decoded = codec.decode(&mut buf).unwrap().unwrap();
    assert_eq!(original, decoded);
}

#[test]
fn roundtrip_resize() {
    let mut codec = FrameCodec;
    let mut buf = BytesMut::new();
    let original = Frame::Resize { cols: 120, rows: 40 };
    codec.encode(original.clone(), &mut buf).unwrap();
    let decoded = codec.decode(&mut buf).unwrap().unwrap();
    assert_eq!(original, decoded);
}

#[test]
fn roundtrip_exit() {
    let mut codec = FrameCodec;
    let mut buf = BytesMut::new();
    let original = Frame::Exit { code: 0 };
    codec.encode(original.clone(), &mut buf).unwrap();
    let decoded = codec.decode(&mut buf).unwrap().unwrap();
    assert_eq!(original, decoded);
}

#[test]
fn decode_incomplete_returns_none() {
    let mut codec = FrameCodec;
    let mut buf = BytesMut::from(&[0x01, 0x00, 0x00][..]);
    assert!(codec.decode(&mut buf).unwrap().is_none());
}

#[test]
fn decode_partial_payload_returns_none() {
    let mut codec = FrameCodec;
    // Header says 5 bytes payload, but only 2 present
    let mut buf = BytesMut::from(&[0x01, 0x00, 0x00, 0x00, 0x05, 0xAA, 0xBB][..]);
    assert!(codec.decode(&mut buf).unwrap().is_none());
}

#[test]
fn decode_invalid_type_returns_error() {
    let mut codec = FrameCodec;
    let mut buf = BytesMut::from(&[0xFF, 0x00, 0x00, 0x00, 0x00][..]);
    assert!(codec.decode(&mut buf).is_err());
}

#[test]
fn roundtrip_new_session() {
    let mut codec = FrameCodec;
    let mut buf = BytesMut::new();
    let original = Frame::NewSession { name: "myproject".to_string() };
    codec.encode(original.clone(), &mut buf).unwrap();
    let decoded = codec.decode(&mut buf).unwrap().unwrap();
    assert_eq!(original, decoded);
}

#[test]
fn roundtrip_new_session_empty_name() {
    let mut codec = FrameCodec;
    let mut buf = BytesMut::new();
    let original = Frame::NewSession { name: String::new() };
    codec.encode(original.clone(), &mut buf).unwrap();
    let decoded = codec.decode(&mut buf).unwrap().unwrap();
    assert_eq!(original, decoded);
}

#[test]
fn roundtrip_attach() {
    let mut codec = FrameCodec;
    let mut buf = BytesMut::new();
    let original = Frame::Attach { session: "0".to_string() };
    codec.encode(original.clone(), &mut buf).unwrap();
    let decoded = codec.decode(&mut buf).unwrap().unwrap();
    assert_eq!(original, decoded);
}

#[test]
fn roundtrip_attach_by_name() {
    let mut codec = FrameCodec;
    let mut buf = BytesMut::new();
    let original = Frame::Attach { session: "myproject".to_string() };
    codec.encode(original.clone(), &mut buf).unwrap();
    let decoded = codec.decode(&mut buf).unwrap().unwrap();
    assert_eq!(original, decoded);
}

#[test]
fn roundtrip_session_created() {
    let mut codec = FrameCodec;
    let mut buf = BytesMut::new();
    let original = Frame::SessionCreated { id: "42".to_string() };
    codec.encode(original.clone(), &mut buf).unwrap();
    let decoded = codec.decode(&mut buf).unwrap().unwrap();
    assert_eq!(original, decoded);
}

#[test]
fn roundtrip_list_sessions() {
    let mut codec = FrameCodec;
    let mut buf = BytesMut::new();
    codec.encode(Frame::ListSessions, &mut buf).unwrap();
    let decoded = codec.decode(&mut buf).unwrap().unwrap();
    assert_eq!(Frame::ListSessions, decoded);
}

#[test]
fn roundtrip_session_info() {
    let mut codec = FrameCodec;
    let mut buf = BytesMut::new();
    let original = Frame::SessionInfo {
        sessions: vec![
            SessionEntry {
                id: "0".to_string(),
                name: "project-a".to_string(),
                pty_path: "/dev/pts/3".to_string(),
                shell_pid: 1234,
                created_at: 1700000000,
                attached: true,
                last_heartbeat: 1700000005,
            },
            SessionEntry {
                id: "1".to_string(),
                name: String::new(),
                pty_path: "/dev/pts/5".to_string(),
                shell_pid: 5678,
                created_at: 1700000100,
                attached: false,
                last_heartbeat: 0,
            },
        ],
    };
    codec.encode(original.clone(), &mut buf).unwrap();
    let decoded = codec.decode(&mut buf).unwrap().unwrap();
    assert_eq!(original, decoded);
}

#[test]
fn roundtrip_session_info_empty() {
    let mut codec = FrameCodec;
    let mut buf = BytesMut::new();
    let original = Frame::SessionInfo { sessions: vec![] };
    codec.encode(original.clone(), &mut buf).unwrap();
    let decoded = codec.decode(&mut buf).unwrap().unwrap();
    assert_eq!(original, decoded);
}

#[test]
fn roundtrip_ok() {
    let mut codec = FrameCodec;
    let mut buf = BytesMut::new();
    codec.encode(Frame::Ok, &mut buf).unwrap();
    let decoded = codec.decode(&mut buf).unwrap().unwrap();
    assert_eq!(Frame::Ok, decoded);
}

#[test]
fn roundtrip_error() {
    let mut codec = FrameCodec;
    let mut buf = BytesMut::new();
    let original = Frame::Error { message: "something failed".to_string() };
    codec.encode(original.clone(), &mut buf).unwrap();
    let decoded = codec.decode(&mut buf).unwrap().unwrap();
    assert_eq!(original, decoded);
}

#[test]
fn roundtrip_detached() {
    let mut codec = FrameCodec;
    let mut buf = BytesMut::new();
    codec.encode(Frame::Detached, &mut buf).unwrap();
    let decoded = codec.decode(&mut buf).unwrap().unwrap();
    assert_eq!(Frame::Detached, decoded);
}

#[test]
fn roundtrip_kill_session() {
    let mut codec = FrameCodec;
    let mut buf = BytesMut::new();
    let original = Frame::KillSession { session: "0".to_string() };
    codec.encode(original.clone(), &mut buf).unwrap();
    let decoded = codec.decode(&mut buf).unwrap().unwrap();
    assert_eq!(original, decoded);
}

#[test]
fn roundtrip_kill_session_by_name() {
    let mut codec = FrameCodec;
    let mut buf = BytesMut::new();
    let original = Frame::KillSession { session: "myproject".to_string() };
    codec.encode(original.clone(), &mut buf).unwrap();
    let decoded = codec.decode(&mut buf).unwrap().unwrap();
    assert_eq!(original, decoded);
}

#[test]
fn roundtrip_kill_server() {
    let mut codec = FrameCodec;
    let mut buf = BytesMut::new();
    codec.encode(Frame::KillServer, &mut buf).unwrap();
    let decoded = codec.decode(&mut buf).unwrap().unwrap();
    assert_eq!(Frame::KillServer, decoded);
}

#[test]
fn multi_frame_decode() {
    let mut codec = FrameCodec;
    let mut buf = BytesMut::new();
    // Encode three different frames into one buffer
    codec.encode(Frame::Data(Bytes::from("abc")), &mut buf).unwrap();
    codec.encode(Frame::Resize { cols: 80, rows: 24 }, &mut buf).unwrap();
    codec.encode(Frame::Exit { code: 7 }, &mut buf).unwrap();

    // Decode them one by one from the same buffer
    assert_eq!(codec.decode(&mut buf).unwrap().unwrap(), Frame::Data(Bytes::from("abc")));
    assert_eq!(codec.decode(&mut buf).unwrap().unwrap(), Frame::Resize { cols: 80, rows: 24 });
    assert_eq!(codec.decode(&mut buf).unwrap().unwrap(), Frame::Exit { code: 7 });
    // Buffer should be empty now
    assert!(codec.decode(&mut buf).unwrap().is_none());
}

#[test]
fn zero_length_data_roundtrip() {
    let mut codec = FrameCodec;
    let mut buf = BytesMut::new();
    let original = Frame::Data(Bytes::new());
    codec.encode(original.clone(), &mut buf).unwrap();
    let decoded = codec.decode(&mut buf).unwrap().unwrap();
    assert_eq!(original, decoded);
}

#[test]
fn roundtrip_exit_negative_code() {
    let mut codec = FrameCodec;
    let mut buf = BytesMut::new();
    let original = Frame::Exit { code: -1 };
    codec.encode(original.clone(), &mut buf).unwrap();
    let decoded = codec.decode(&mut buf).unwrap().unwrap();
    assert_eq!(original, decoded);
}

#[test]
fn roundtrip_error_empty_message() {
    let mut codec = FrameCodec;
    let mut buf = BytesMut::new();
    let original = Frame::Error { message: String::new() };
    codec.encode(original.clone(), &mut buf).unwrap();
    let decoded = codec.decode(&mut buf).unwrap().unwrap();
    assert_eq!(original, decoded);
}

#[test]
fn resize_wrong_payload_size_too_short() {
    let mut codec = FrameCodec;
    // Resize frame type (0x02) with only 3 bytes payload instead of 4
    let mut buf = BytesMut::from(&[0x02, 0x00, 0x00, 0x00, 0x03, 0x00, 0x50, 0x00][..]);
    let err = codec.decode(&mut buf).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn exit_wrong_payload_size() {
    let mut codec = FrameCodec;
    // Exit frame type (0x03) with 2 bytes payload instead of 4
    let mut buf = BytesMut::from(&[0x03, 0x00, 0x00, 0x00, 0x02, 0x00, 0x2A][..]);
    let err = codec.decode(&mut buf).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn session_info_with_tabs_in_id() {
    // Tabs in id would corrupt the tab-separated wire format
    let mut codec = FrameCodec;
    let mut buf = BytesMut::new();
    let original = Frame::SessionInfo {
        sessions: vec![SessionEntry {
            id: "has\ttab".to_string(),
            name: "test".to_string(),
            pty_path: "/dev/pts/3".to_string(),
            shell_pid: 1234,
            created_at: 1700000000,
            attached: true,
            last_heartbeat: 0,
        }],
    };
    codec.encode(original.clone(), &mut buf).unwrap();
    let decoded = codec.decode(&mut buf).unwrap().unwrap();
    // The tab splits the field incorrectly — 8 fields instead of 7, so filter_map drops the line
    match decoded {
        Frame::SessionInfo { sessions } => {
            assert_eq!(
                sessions.len(),
                0,
                "tab in id corrupts wire format — entry should be dropped"
            );
        }
        other => panic!("expected SessionInfo, got {other:?}"),
    }
}

#[test]
fn large_data_frame_roundtrip() {
    let mut codec = FrameCodec;
    let mut buf = BytesMut::new();
    let payload = vec![0xABu8; 64 * 1024]; // 64KB
    let original = Frame::Data(Bytes::from(payload));
    codec.encode(original.clone(), &mut buf).unwrap();
    let decoded = codec.decode(&mut buf).unwrap().unwrap();
    assert_eq!(original, decoded);
}

#[test]
fn decode_empty_buffer_returns_none() {
    let mut codec = FrameCodec;
    let mut buf = BytesMut::new();
    assert!(codec.decode(&mut buf).unwrap().is_none());
}

#[test]
fn decode_consumes_only_one_frame() {
    let mut codec = FrameCodec;
    let mut buf = BytesMut::new();
    codec.encode(Frame::Data(Bytes::from("first")), &mut buf).unwrap();
    codec.encode(Frame::Data(Bytes::from("second")), &mut buf).unwrap();
    let total_len = buf.len();

    let first = codec.decode(&mut buf).unwrap().unwrap();
    assert_eq!(first, Frame::Data(Bytes::from("first")));
    // Buffer should still have the second frame
    assert!(buf.len() < total_len);
    assert!(!buf.is_empty());

    let second = codec.decode(&mut buf).unwrap().unwrap();
    assert_eq!(second, Frame::Data(Bytes::from("second")));
    assert!(buf.is_empty());
}

#[test]
fn session_info_with_newline_in_name() {
    // Newlines in names corrupt the line-separated wire format.
    // Wire: "0\thas\nnewline\t/dev/pts/3\t1234\t1700000000\t1\t0"
    // Splits into two lines, neither has 7 fields, so both are dropped.
    let mut codec = FrameCodec;
    let mut buf = BytesMut::new();
    let original = Frame::SessionInfo {
        sessions: vec![SessionEntry {
            id: "0".to_string(),
            name: "has\nnewline".to_string(),
            pty_path: "/dev/pts/3".to_string(),
            shell_pid: 1234,
            created_at: 1700000000,
            attached: true,
            last_heartbeat: 0,
        }],
    };
    codec.encode(original, &mut buf).unwrap();
    let decoded = codec.decode(&mut buf).unwrap().unwrap();
    match decoded {
        Frame::SessionInfo { sessions } => {
            assert_eq!(
                sessions.len(),
                0,
                "newline in name corrupts wire format — entry should be dropped"
            );
        }
        other => panic!("expected SessionInfo, got {other:?}"),
    }
}

#[test]
fn invalid_utf8_in_string_frame() {
    let mut codec = FrameCodec;
    // NewSession (0x10) with invalid UTF-8 payload
    let mut buf = BytesMut::new();
    buf.put_u8(0x10); // TYPE_NEW_SESSION
    buf.put_u32(2);
    buf.put_slice(&[0xFF, 0xFE]); // invalid UTF-8
    let err = codec.decode(&mut buf).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn roundtrip_ping() {
    let mut codec = FrameCodec;
    let mut buf = BytesMut::new();
    codec.encode(Frame::Ping, &mut buf).unwrap();
    assert_eq!(buf.len(), 5); // type(1) + len(4), zero payload
    assert_eq!(buf[0], 0x05);
    let decoded = codec.decode(&mut buf).unwrap().unwrap();
    assert_eq!(Frame::Ping, decoded);
}

#[test]
fn roundtrip_pong() {
    let mut codec = FrameCodec;
    let mut buf = BytesMut::new();
    codec.encode(Frame::Pong, &mut buf).unwrap();
    assert_eq!(buf.len(), 5); // type(1) + len(4), zero payload
    assert_eq!(buf[0], 0x06);
    let decoded = codec.decode(&mut buf).unwrap().unwrap();
    assert_eq!(Frame::Pong, decoded);
}

#[test]
fn roundtrip_env() {
    let mut codec = FrameCodec;
    let mut buf = BytesMut::new();
    let original = Frame::Env {
        vars: vec![
            ("TERM".to_string(), "xterm-256color".to_string()),
            ("LANG".to_string(), "en_US.UTF-8".to_string()),
            ("COLORTERM".to_string(), "truecolor".to_string()),
        ],
    };
    codec.encode(original.clone(), &mut buf).unwrap();
    assert_eq!(buf[0], 0x07);
    let decoded = codec.decode(&mut buf).unwrap().unwrap();
    assert_eq!(original, decoded);
}

#[test]
fn decode_oversized_frame_rejected() {
    let mut codec = FrameCodec;
    let mut buf = BytesMut::new();
    // Header claiming payload of 1 MB + 1 byte (exceeds MAX_FRAME_SIZE)
    buf.put_u8(0x01); // TYPE_DATA
    buf.put_u32((1 << 20) + 1); // 1_048_577 bytes
    let err = codec.decode(&mut buf).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    assert!(err.to_string().contains("too large"));
}

#[test]
fn decode_exactly_max_frame_size_accepted() {
    let mut codec = FrameCodec;
    let mut buf = BytesMut::new();
    let payload = vec![0x42u8; 1 << 20]; // exactly 1 MB
    buf.put_u8(0x01); // TYPE_DATA
    buf.put_u32(1 << 20);
    buf.put_slice(&payload);
    let decoded = codec.decode(&mut buf).unwrap().unwrap();
    assert_eq!(decoded, Frame::Data(Bytes::from(payload)));
}

#[test]
fn roundtrip_env_empty() {
    let mut codec = FrameCodec;
    let mut buf = BytesMut::new();
    let original = Frame::Env { vars: vec![] };
    codec.encode(original.clone(), &mut buf).unwrap();
    assert_eq!(buf.len(), 5); // type(1) + len(4), zero payload
    assert_eq!(buf[0], 0x07);
    let decoded = codec.decode(&mut buf).unwrap().unwrap();
    assert_eq!(original, decoded);
}
