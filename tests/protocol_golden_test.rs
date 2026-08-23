//! Golden wire vectors: one encoded instance per `Frame` variant, byte-exact.
//!
//! Roundtrip tests prove `decode(encode(x)) == x`; they cannot notice an
//! encoding change that moves encoder and decoder together. These vectors
//! pin the bytes a v-current peer actually emits, so such a change fails
//! here -- and the fix is to bump `PROTOCOL_VERSION`, update
//! docs/wire-protocol.md, and regenerate the table (the failure message
//! prints it). `variant_name` is an exhaustive match: a new `Frame` variant
//! does not compile until it has a sample.

use bytes::{Bytes, BytesMut};
use gritty::protocol::{ErrorCode, Frame, FrameCodec, SessionEntry};
use tokio_util::codec::Encoder;

fn samples() -> Vec<Frame> {
    vec![
        Frame::Data(Bytes::from_static(b"hi")),
        Frame::Resize { cols: 80, rows: 24 },
        Frame::Exit { code: -1 },
        Frame::Detached,
        Frame::Ping,
        Frame::Pong,
        Frame::Env { vars: vec![("TERM".into(), "xterm".into()), ("K".into(), String::new())] },
        Frame::DiagRequest,
        Frame::DiagResponse { text: "ok".into() },
        Frame::ServerShutdown,
        Frame::Resume { offset: 0x0102_0304_0506_0708 },
        Frame::Notice(Bytes::from_static(b"n")),
        Frame::AgentForward,
        Frame::AgentOpen { channel_id: 1 },
        Frame::AgentData { channel_id: 2, data: Bytes::from_static(b"ad") },
        Frame::AgentClose { channel_id: 3 },
        Frame::ClipboardSet { data: Bytes::from_static(b"cs") },
        Frame::ClipboardGet,
        Frame::ClipboardData { data: Bytes::from_static(b"cd") },
        Frame::OpenForward,
        Frame::OpenUrl { url: "u".into() },
        Frame::TunnelListen { port: 0x1f90 },
        Frame::TunnelOpen { channel_id: 4 },
        Frame::TunnelData { channel_id: 5, data: Bytes::from_static(b"td") },
        Frame::TunnelClose { channel_id: 6 },
        Frame::SendOffer { file_count: 3, total_bytes: 0x0a0b },
        Frame::SendDone,
        Frame::SendCancel { reason: "r".into() },
        Frame::PortForwardListen { forward_id: 1, listen_port: 2, target_port: 3 },
        Frame::PortForwardReady { forward_id: 4 },
        Frame::PortForwardOpen { forward_id: 5, channel_id: 6, target_port: 7 },
        Frame::PortForwardData { channel_id: 8, data: Bytes::from_static(b"pd") },
        Frame::PortForwardClose { channel_id: 9 },
        Frame::PortForwardStop { forward_id: 10 },
        Frame::PortForwardRequest {
            forward_id: 11,
            direction: 1,
            listen_port: 12,
            target_port: 13,
        },
        Frame::Hello { version: 0x0102, capabilities: 0x0304_0506, device_id: 7 },
        Frame::HelloAck { version: 0x0102, capabilities: 0x0304_0506, server_id: 8 },
        Frame::NewSession {
            name: "n".into(),
            command: "c".into(),
            cwd: "/d".into(),
            cols: 1,
            rows: 2,
            client_name: "cl".into(),
            linger_secs: 3,
        },
        Frame::Attach {
            session: "s".into(),
            client_name: "cl".into(),
            force: true,
            no_replay: false,
            cols: 1,
            rows: 2,
            attach_token: 3,
            rendered_offset: 4,
            line_dirty: true,
        },
        Frame::AttachAck { token: 1, session_id: 2 },
        Frame::SessionCreated { id: 9 },
        Frame::ListSessions,
        Frame::SessionInfo {
            sessions: vec![SessionEntry {
                id: 1,
                name: "n".into(),
                pty_path: "/p".into(),
                shell_pid: 2,
                created_at: 3,
                attached: true,
                last_heartbeat: 4,
                foreground_cmd: "f".into(),
                cwd: "/c".into(),
                client_name: "cl".into(),
                agent_forwarding_active: false,
                is_last_attached: true,
                last_activity: 5,
                linger_secs: 6,
            }],
        },
        Frame::Ok,
        Frame::Error { code: ErrorCode::OwnerChanged, message: "m".into() },
        Frame::KillSession { session: "s".into() },
        Frame::KillServer,
        Frame::RenameSession { session: "s".into(), new_name: "t".into() },
        Frame::SetLinger { session: "s".into(), linger_secs: 7 },
        Frame::Tail { session: "s".into() },
        Frame::SendFile { session: "s".into() },
    ]
}

/// Exhaustive on purpose: adding a `Frame` variant must add a sample.
fn variant_name(frame: &Frame) -> &'static str {
    match frame {
        Frame::Data(_) => "Data",
        Frame::Resize { .. } => "Resize",
        Frame::Exit { .. } => "Exit",
        Frame::Detached => "Detached",
        Frame::Ping => "Ping",
        Frame::Pong => "Pong",
        Frame::Env { .. } => "Env",
        Frame::DiagRequest => "DiagRequest",
        Frame::DiagResponse { .. } => "DiagResponse",
        Frame::ServerShutdown => "ServerShutdown",
        Frame::Resume { .. } => "Resume",
        Frame::Notice(_) => "Notice",
        Frame::AgentForward => "AgentForward",
        Frame::AgentOpen { .. } => "AgentOpen",
        Frame::AgentData { .. } => "AgentData",
        Frame::AgentClose { .. } => "AgentClose",
        Frame::ClipboardSet { .. } => "ClipboardSet",
        Frame::ClipboardGet => "ClipboardGet",
        Frame::ClipboardData { .. } => "ClipboardData",
        Frame::OpenForward => "OpenForward",
        Frame::OpenUrl { .. } => "OpenUrl",
        Frame::TunnelListen { .. } => "TunnelListen",
        Frame::TunnelOpen { .. } => "TunnelOpen",
        Frame::TunnelData { .. } => "TunnelData",
        Frame::TunnelClose { .. } => "TunnelClose",
        Frame::SendOffer { .. } => "SendOffer",
        Frame::SendDone => "SendDone",
        Frame::SendCancel { .. } => "SendCancel",
        Frame::PortForwardListen { .. } => "PortForwardListen",
        Frame::PortForwardReady { .. } => "PortForwardReady",
        Frame::PortForwardOpen { .. } => "PortForwardOpen",
        Frame::PortForwardData { .. } => "PortForwardData",
        Frame::PortForwardClose { .. } => "PortForwardClose",
        Frame::PortForwardStop { .. } => "PortForwardStop",
        Frame::PortForwardRequest { .. } => "PortForwardRequest",
        Frame::Hello { .. } => "Hello",
        Frame::HelloAck { .. } => "HelloAck",
        Frame::NewSession { .. } => "NewSession",
        Frame::Attach { .. } => "Attach",
        Frame::AttachAck { .. } => "AttachAck",
        Frame::SessionCreated { .. } => "SessionCreated",
        Frame::ListSessions => "ListSessions",
        Frame::SessionInfo { .. } => "SessionInfo",
        Frame::Ok => "Ok",
        Frame::Error { .. } => "Error",
        Frame::KillSession { .. } => "KillSession",
        Frame::KillServer => "KillServer",
        Frame::RenameSession { .. } => "RenameSession",
        Frame::SetLinger { .. } => "SetLinger",
        Frame::Tail { .. } => "Tail",
        Frame::SendFile { .. } => "SendFile",
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn encode(frame: Frame) -> String {
    let mut buf = BytesMut::new();
    FrameCodec.encode(frame, &mut buf).unwrap();
    hex(&buf)
}

#[test]
fn every_variant_has_exactly_one_sample() {
    let mut names: Vec<&str> = samples().iter().map(variant_name).collect();
    let n = names.len();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), n, "duplicate sample variants");
    assert_eq!(n, GOLDEN.len(), "sample count and golden table disagree");
}

#[test]
fn encodings_match_the_golden_vectors() {
    let actual: Vec<(&str, String)> =
        samples().into_iter().map(|f| (variant_name(&f), encode(f))).collect();
    let mismatches: Vec<String> = actual
        .iter()
        .filter(|(name, bytes)| {
            GOLDEN.iter().find(|(n, _)| n == name).map(|(_, g)| g != bytes).unwrap_or(true)
        })
        .map(|(name, bytes)| format!("  {name}: {bytes}"))
        .collect();
    if !mismatches.is_empty() {
        let table: Vec<String> =
            actual.iter().map(|(n, b)| format!("    (\"{n}\", \"{b}\"),")).collect();
        panic!(
            "wire encoding changed for:\n{}\n\nThis is a protocol change: bump PROTOCOL_VERSION, \
             update docs/wire-protocol.md, then replace GOLDEN with:\n{}",
            mismatches.join("\n"),
            table.join("\n")
        );
    }
}

/// Regenerate by running the test above and pasting the table it prints.
const GOLDEN: &[(&str, &str)] = &[
    ("Data", "10000000026869"),
    ("Resize", "110000000400500018"),
    ("Exit", "1200000004ffffffff"),
    ("Detached", "1300000000"),
    ("Ping", "1400000000"),
    ("Pong", "1500000000"),
    ("Env", "16000000160000000200045445524d0005787465726d00014b0000"),
    ("DiagRequest", "1700000000"),
    ("DiagResponse", "18000000026f6b"),
    ("ServerShutdown", "1900000000"),
    ("Resume", "1a000000080102030405060708"),
    ("Notice", "1b000000016e"),
    ("AgentForward", "2000000000"),
    ("AgentOpen", "210000000400000001"),
    ("AgentData", "2200000006000000026164"),
    ("AgentClose", "230000000400000003"),
    ("ClipboardSet", "2a000000026373"),
    ("ClipboardGet", "2b00000000"),
    ("ClipboardData", "2c000000026364"),
    ("OpenForward", "2800000000"),
    ("OpenUrl", "290000000175"),
    ("TunnelListen", "30000000021f90"),
    ("TunnelOpen", "310000000400000004"),
    ("TunnelData", "3200000006000000057464"),
    ("TunnelClose", "330000000400000006"),
    ("SendOffer", "380000000c000000030000000000000a0b"),
    ("SendDone", "3900000000"),
    ("SendCancel", "3a0000000172"),
    ("PortForwardListen", "40000000080000000100020003"),
    ("PortForwardReady", "410000000400000004"),
    ("PortForwardOpen", "420000000a00000005000000060007"),
    ("PortForwardData", "4300000006000000087064"),
    ("PortForwardClose", "440000000400000009"),
    ("PortForwardStop", "45000000040000000a"),
    ("PortForwardRequest", "46000000090000000b01000c000d"),
    ("Hello", "010000000e0102030405060000000000000007"),
    ("HelloAck", "020000000e0102030405060000000000000008"),
    ("NewSession", "500000001a00016e00016300022f64000100020002636c0000000000000003"),
    ("Attach", "510000001e0001730002636c0100000100020000000000000003000000000000000401"),
    ("AttachAck", "640000000c000000000000000100000002"),
    ("SessionCreated", "600000000400000009"),
    ("ListSessions", "5200000000"),
    (
        "SessionInfo",
        "6100000045000000010000003d0000000100016e00022f7000000002000000000000000301000000000000000400016600022f630002636c000100000000000000050000000000000006",
    ),
    ("Ok", "6200000000"),
    ("Error", "630000000300086d"),
    ("KillSession", "530000000173"),
    ("KillServer", "5400000000"),
    ("RenameSession", "560000000400017374"),
    ("SetLinger", "570000000b0001730000000000000007"),
    ("Tail", "550000000173"),
    ("SendFile", "3b0000000173"),
];
