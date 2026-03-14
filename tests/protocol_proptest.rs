use bytes::{Bytes, BytesMut};
use gritty::protocol::{ErrorCode, Frame, FrameCodec, SessionEntry};
use proptest::prelude::*;
use tokio_util::codec::{Decoder, Encoder};

fn arb_bytes() -> impl Strategy<Value = Bytes> {
    prop::collection::vec(any::<u8>(), 0..256).prop_map(Bytes::from)
}

fn arb_string() -> impl Strategy<Value = String> {
    "[a-z0-9]{0,100}"
}

fn arb_error_code() -> impl Strategy<Value = ErrorCode> {
    prop_oneof![
        Just(ErrorCode::NoSuchSession),
        Just(ErrorCode::NameAlreadyExists),
        Just(ErrorCode::InvalidName),
        Just(ErrorCode::EmptyName),
        Just(ErrorCode::VersionMismatch),
        Just(ErrorCode::UnexpectedFrame),
        Just(ErrorCode::AlreadyAttached),
        (8u16..=1000u16).prop_map(ErrorCode::Unknown),
    ]
}

fn arb_session_entry() -> impl Strategy<Value = SessionEntry> {
    (
        any::<u32>(),
        arb_string(),
        arb_string(),
        any::<u32>(),
        any::<u64>(),
        any::<bool>(),
        any::<u64>(),
        arb_string(),
        arb_string(),
        arb_string(),
    )
        .prop_map(
            |(
                id,
                name,
                pty_path,
                shell_pid,
                created_at,
                attached,
                last_heartbeat,
                foreground_cmd,
                cwd,
                client_name,
            )| {
                SessionEntry {
                    id,
                    name,
                    pty_path,
                    shell_pid,
                    created_at,
                    attached,
                    last_heartbeat,
                    foreground_cmd,
                    cwd,
                    client_name,
                }
            },
        )
}

fn arb_env_vars() -> impl Strategy<Value = Vec<(String, String)>> {
    prop::collection::vec((arb_string(), arb_string()), 0..8)
}

fn arb_frame() -> impl Strategy<Value = Frame> {
    prop_oneof![
        // Blob frames
        arb_bytes().prop_map(Frame::Data),
        arb_bytes().prop_map(|data| Frame::ClipboardSet { data }),
        arb_bytes().prop_map(|data| Frame::ClipboardData { data }),
        // Fixed-field frames
        (any::<u16>(), any::<u16>()).prop_map(|(cols, rows)| Frame::Resize { cols, rows }),
        any::<i32>().prop_map(|code| Frame::Exit { code }),
        (any::<u16>(), any::<u32>())
            .prop_map(|(version, capabilities)| Frame::Hello { version, capabilities }),
        (any::<u16>(), any::<u32>())
            .prop_map(|(version, capabilities)| Frame::HelloAck { version, capabilities }),
        any::<u32>().prop_map(|channel_id| Frame::AgentOpen { channel_id }),
        any::<u32>().prop_map(|channel_id| Frame::AgentClose { channel_id }),
        any::<u16>().prop_map(|port| Frame::TunnelListen { port }),
        any::<u32>().prop_map(|channel_id| Frame::TunnelOpen { channel_id }),
        any::<u32>().prop_map(|channel_id| Frame::TunnelClose { channel_id }),
        (any::<u32>(), any::<u64>())
            .prop_map(|(file_count, total_bytes)| Frame::SendOffer { file_count, total_bytes }),
        (any::<u32>(), any::<u16>(), any::<u16>()).prop_map(
            |(forward_id, listen_port, target_port)| Frame::PortForwardListen {
                forward_id,
                listen_port,
                target_port,
            }
        ),
        any::<u32>().prop_map(|forward_id| Frame::PortForwardReady { forward_id }),
        (any::<u32>(), any::<u32>(), any::<u16>()).prop_map(
            |(forward_id, channel_id, target_port)| Frame::PortForwardOpen {
                forward_id,
                channel_id,
                target_port,
            }
        ),
        any::<u32>().prop_map(|channel_id| Frame::PortForwardClose { channel_id }),
        any::<u32>().prop_map(|forward_id| Frame::PortForwardStop { forward_id }),
        any::<u32>().prop_map(|id| Frame::SessionCreated { id }),
        // Prefix + blob frames
        (any::<u32>(), arb_bytes())
            .prop_map(|(channel_id, data)| Frame::AgentData { channel_id, data }),
        (any::<u32>(), arb_bytes())
            .prop_map(|(channel_id, data)| Frame::TunnelData { channel_id, data }),
        (any::<u32>(), arb_bytes())
            .prop_map(|(channel_id, data)| Frame::PortForwardData { channel_id, data }),
        // Empty frames
        Just(Frame::Detached),
        Just(Frame::Ping),
        Just(Frame::Pong),
        Just(Frame::AgentForward),
        Just(Frame::OpenForward),
        Just(Frame::ClipboardGet),
        Just(Frame::SendDone),
        Just(Frame::ListSessions),
        Just(Frame::KillServer),
        Just(Frame::Ok),
        // String frames
        arb_string().prop_map(|url| Frame::OpenUrl { url }),
        arb_string().prop_map(|reason| Frame::SendCancel { reason }),
        arb_string().prop_map(|session| Frame::Tail { session }),
        arb_string().prop_map(|session| Frame::KillSession { session }),
        arb_string().prop_map(|session| Frame::SendFile { session }),
        (arb_string(), arb_string())
            .prop_map(|(session, new_name)| Frame::RenameSession { session, new_name }),
        // Structured frames
        (arb_string(), arb_string(), arb_string(), any::<u16>(), any::<u16>(), arb_string())
            .prop_map(|(name, command, cwd, cols, rows, client_name)| Frame::NewSession {
                name,
                command,
                cwd,
                cols,
                rows,
                client_name,
            }),
        (arb_string(), arb_string(), any::<bool>()).prop_map(|(session, client_name, force)| {
            Frame::Attach { session, client_name, force }
        }),
        (arb_error_code(), arb_string()).prop_map(|(code, message)| Frame::Error { code, message }),
        arb_env_vars().prop_map(|vars| Frame::Env { vars }),
        prop::collection::vec(arb_session_entry(), 0..4)
            .prop_map(|sessions| Frame::SessionInfo { sessions }),
    ]
}

proptest! {
    #[test]
    fn decoder_never_panics(data: Vec<u8>) {
        let mut codec = FrameCodec;
        let mut buf = BytesMut::from(&data[..]);
        loop {
            match codec.decode(&mut buf) {
                Ok(None) | Err(_) => break,
                Ok(Some(_)) => continue,
            }
        }
    }

    #[test]
    fn frame_roundtrip(frame in arb_frame()) {
        let mut codec = FrameCodec;
        let mut buf = BytesMut::new();
        codec.encode(frame.clone(), &mut buf).unwrap();
        let decoded = codec.decode(&mut buf).unwrap().unwrap();
        prop_assert_eq!(frame, decoded);
        prop_assert!(buf.is_empty());
    }
}
