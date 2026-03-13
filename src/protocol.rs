use bytes::{Buf, BufMut, Bytes, BytesMut};
use std::io;
use tokio_util::codec::{Decoder, Encoder};

// Handshake
const TYPE_HELLO: u8 = 0x01;
const TYPE_HELLO_ACK: u8 = 0x02;

// Session relay
const TYPE_DATA: u8 = 0x10;
const TYPE_RESIZE: u8 = 0x11;
const TYPE_EXIT: u8 = 0x12;
const TYPE_DETACHED: u8 = 0x13;
const TYPE_PING: u8 = 0x14;
const TYPE_PONG: u8 = 0x15;
const TYPE_ENV: u8 = 0x16;

// Agent forwarding
const TYPE_AGENT_FORWARD: u8 = 0x20;
const TYPE_AGENT_OPEN: u8 = 0x21;
const TYPE_AGENT_DATA: u8 = 0x22;
const TYPE_AGENT_CLOSE: u8 = 0x23;

// URL/browser
const TYPE_OPEN_FORWARD: u8 = 0x28;
const TYPE_OPEN_URL: u8 = 0x29;

// Clipboard
const TYPE_CLIPBOARD_SET: u8 = 0x2A;
const TYPE_CLIPBOARD_GET: u8 = 0x2B;
const TYPE_CLIPBOARD_DATA: u8 = 0x2C;

// Tunnel
const TYPE_TUNNEL_LISTEN: u8 = 0x30;
const TYPE_TUNNEL_OPEN: u8 = 0x31;
const TYPE_TUNNEL_DATA: u8 = 0x32;
const TYPE_TUNNEL_CLOSE: u8 = 0x33;

// File transfer
const TYPE_SEND_OFFER: u8 = 0x38;
const TYPE_SEND_DONE: u8 = 0x39;
const TYPE_SEND_CANCEL: u8 = 0x3A;
const TYPE_SEND_FILE: u8 = 0x3B;

// Port forwarding
const TYPE_PORT_FORWARD_LISTEN: u8 = 0x40;
const TYPE_PORT_FORWARD_READY: u8 = 0x41;
const TYPE_PORT_FORWARD_OPEN: u8 = 0x42;
const TYPE_PORT_FORWARD_DATA: u8 = 0x43;
const TYPE_PORT_FORWARD_CLOSE: u8 = 0x44;
const TYPE_PORT_FORWARD_STOP: u8 = 0x45;

// Control requests
const TYPE_NEW_SESSION: u8 = 0x50;
const TYPE_ATTACH: u8 = 0x51;
const TYPE_LIST_SESSIONS: u8 = 0x52;
const TYPE_KILL_SESSION: u8 = 0x53;
const TYPE_KILL_SERVER: u8 = 0x54;
const TYPE_TAIL: u8 = 0x55;
const TYPE_RENAME_SESSION: u8 = 0x56;

// Control responses
const TYPE_SESSION_CREATED: u8 = 0x60;
const TYPE_SESSION_INFO: u8 = 0x61;
const TYPE_OK: u8 = 0x62;
const TYPE_ERROR: u8 = 0x63;

const HEADER_LEN: usize = 5; // type(1) + length(4)
const MAX_FRAME_SIZE: usize = 1 << 20; // 1 MB

/// Protocol version for handshake negotiation.
pub const PROTOCOL_VERSION: u16 = 9;

/// Structured error codes for forward-compatible error handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    NoSuchSession,
    NameAlreadyExists,
    InvalidName,
    EmptyName,
    VersionMismatch,
    UnexpectedFrame,
    AlreadyAttached,
    Unknown(u16),
}

impl ErrorCode {
    pub fn to_u16(self) -> u16 {
        match self {
            Self::NoSuchSession => 1,
            Self::NameAlreadyExists => 2,
            Self::InvalidName => 3,
            Self::EmptyName => 4,
            Self::VersionMismatch => 5,
            Self::UnexpectedFrame => 6,
            Self::AlreadyAttached => 7,
            Self::Unknown(v) => v,
        }
    }

    pub fn from_u16(v: u16) -> Self {
        match v {
            1 => Self::NoSuchSession,
            2 => Self::NameAlreadyExists,
            3 => Self::InvalidName,
            4 => Self::EmptyName,
            5 => Self::VersionMismatch,
            6 => Self::UnexpectedFrame,
            7 => Self::AlreadyAttached,
            _ => Self::Unknown(v),
        }
    }
}

/// Discriminator byte for the unified per-session service socket (`svc-{id}.sock`).
/// Sent as the first byte on every connection to route to the correct handler.
#[repr(u8)]
pub enum SvcRequest {
    OpenUrl = 1,
    Send = 2,
    Receive = 3,
    PortForward = 4,
    Clipboard = 5,
}

impl SvcRequest {
    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            1 => Some(Self::OpenUrl),
            2 => Some(Self::Send),
            3 => Some(Self::Receive),
            4 => Some(Self::PortForward),
            5 => Some(Self::Clipboard),
            _ => None,
        }
    }

    pub fn to_byte(self) -> u8 {
        self as u8
    }
}

/// Metadata for one session, returned in SessionInfo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEntry {
    pub id: u32,
    pub name: String,
    pub pty_path: String,
    pub shell_pid: u32,
    pub created_at: u64,
    pub attached: bool,
    pub last_heartbeat: u64,
    pub foreground_cmd: String,
    pub cwd: String,
    pub client_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    Data(Bytes),
    Resize {
        cols: u16,
        rows: u16,
    },
    Exit {
        code: i32,
    },
    /// Sent to a client when another client takes over the session.
    Detached,
    /// Heartbeat request (client → server).
    Ping,
    /// Heartbeat reply (server → client).
    Pong,
    /// Environment variables (client → server, sent before first Resize on new session).
    Env {
        vars: Vec<(String, String)>,
    },
    /// Client signals it can handle agent forwarding (client → server).
    AgentForward,
    /// New agent connection on the remote side (server → client).
    AgentOpen {
        channel_id: u32,
    },
    /// Agent protocol data (bidirectional).
    AgentData {
        channel_id: u32,
        data: Bytes,
    },
    /// Close an agent channel (bidirectional).
    AgentClose {
        channel_id: u32,
    },
    /// Server tells client to put data in the clipboard (server → client).
    ClipboardSet {
        data: Bytes,
    },
    /// Server asks client for clipboard contents (server → client).
    ClipboardGet,
    /// Client sends clipboard contents to server (client → server).
    ClipboardData {
        data: Bytes,
    },
    /// Client signals it can handle URL open forwarding (client → server).
    OpenForward,
    /// URL to open on the client machine (server → client).
    OpenUrl {
        url: String,
    },
    /// Server asks client to bind a local TCP port for reverse tunneling (server → client).
    TunnelListen {
        port: u16,
    },
    /// Client signals a tunnel connection has been accepted (client → server).
    TunnelOpen {
        channel_id: u32,
    },
    /// Tunnel data relay (bidirectional).
    TunnelData {
        channel_id: u32,
        data: Bytes,
    },
    /// Tunnel connection closed (bidirectional).
    TunnelClose {
        channel_id: u32,
    },
    /// Server notifies attached client that a file transfer started (server → client).
    SendOffer {
        file_count: u32,
        total_bytes: u64,
    },
    /// Server notifies attached client that a file transfer completed (server → client).
    SendDone,
    /// File transfer cancelled (server → client).
    SendCancel {
        reason: String,
    },
    /// Server asks client to set up a port forward listener (server → client for remote-fwd).
    PortForwardListen {
        forward_id: u32,
        listen_port: u16,
        target_port: u16,
    },
    /// Client confirms port forward listener is ready (client → server).
    PortForwardReady {
        forward_id: u32,
    },
    /// New TCP connection on a port forward (bidirectional).
    PortForwardOpen {
        forward_id: u32,
        channel_id: u32,
        target_port: u16,
    },
    /// Port forward channel data (bidirectional).
    PortForwardData {
        channel_id: u32,
        data: Bytes,
    },
    /// Close a port forward channel (bidirectional).
    PortForwardClose {
        channel_id: u32,
    },
    /// Tear down an entire port forward (server → client).
    PortForwardStop {
        forward_id: u32,
    },
    /// Protocol version handshake (client → server, first frame on connection).
    Hello {
        version: u16,
        capabilities: u32,
    },
    /// Protocol version acknowledgement (server → client).
    HelloAck {
        version: u16,
        capabilities: u32,
    },
    // Control requests
    /// Local-side file transfer routing (client → daemon).
    SendFile {
        session: String,
    },
    NewSession {
        name: String,
        command: String,
        cwd: String,
        cols: u16,
        rows: u16,
        client_name: String,
    },
    Attach {
        session: String,
        client_name: String,
        force: bool,
    },
    /// Read-only tail of a session's PTY output (client → server).
    Tail {
        session: String,
    },
    ListSessions,
    KillSession {
        session: String,
    },
    KillServer,
    RenameSession {
        session: String,
        new_name: String,
    },
    // Control responses
    SessionCreated {
        id: u32,
    },
    SessionInfo {
        sessions: Vec<SessionEntry>,
    },
    Ok,
    Error {
        code: ErrorCode,
        message: String,
    },
}

impl Frame {
    /// Extract a Frame from a `framed.next().await` result, converting
    /// the common None / Some(Err) cases into descriptive errors.
    pub fn expect_from(result: Option<Result<Frame, io::Error>>) -> anyhow::Result<Frame> {
        match result {
            Some(Ok(frame)) => Ok(frame),
            Some(Err(e)) => Err(anyhow::anyhow!("daemon protocol error: {e}")),
            None => Err(anyhow::anyhow!("daemon closed connection")),
        }
    }
}

pub struct FrameCodec;

fn encode_empty(dst: &mut BytesMut, ty: u8) {
    dst.put_u8(ty);
    dst.put_u32(0);
}

fn encode_str(dst: &mut BytesMut, ty: u8, s: &str) {
    dst.put_u8(ty);
    dst.put_u32(s.len() as u32);
    dst.extend_from_slice(s.as_bytes());
}

fn encode_blob(dst: &mut BytesMut, ty: u8, data: &[u8]) {
    dst.put_u8(ty);
    dst.put_u32(data.len() as u32);
    dst.extend_from_slice(data);
}

fn encode_prefix_blob(dst: &mut BytesMut, ty: u8, prefix: u32, data: &[u8]) {
    dst.put_u8(ty);
    dst.put_u32(4 + data.len() as u32);
    dst.put_u32(prefix);
    dst.extend_from_slice(data);
}

fn encode_env(dst: &mut BytesMut, vars: &[(String, String)]) {
    let body_len: usize = 4 + vars.iter().map(|(k, v)| 2 + k.len() + 2 + v.len()).sum::<usize>();
    dst.put_u8(TYPE_ENV);
    dst.put_u32(body_len as u32);
    dst.put_u32(vars.len() as u32);
    for (k, v) in vars {
        dst.put_u16(k.len() as u16);
        dst.extend_from_slice(k.as_bytes());
        dst.put_u16(v.len() as u16);
        dst.extend_from_slice(v.as_bytes());
    }
}

fn encode_session_info(dst: &mut BytesMut, sessions: &[SessionEntry]) {
    // Compute total body length: count(4) + sum of (entry_len(4) + entry_bytes)
    let body_len: usize = 4 + sessions
        .iter()
        .map(|e| {
            4 + entry_encoded_len(e) // entry_len prefix + entry content
        })
        .sum::<usize>();
    dst.put_u8(TYPE_SESSION_INFO);
    dst.put_u32(body_len as u32);
    dst.put_u32(sessions.len() as u32);
    for e in sessions {
        let elen = entry_encoded_len(e) as u32;
        dst.put_u32(elen);
        // id: u32
        dst.put_u32(e.id);
        // name: len-prefixed string
        dst.put_u16(e.name.len() as u16);
        dst.extend_from_slice(e.name.as_bytes());
        dst.put_u16(e.pty_path.len() as u16);
        dst.extend_from_slice(e.pty_path.as_bytes());
        dst.put_u32(e.shell_pid);
        dst.put_u64(e.created_at);
        dst.put_u8(if e.attached { 1 } else { 0 });
        dst.put_u64(e.last_heartbeat);
        dst.put_u16(e.foreground_cmd.len() as u16);
        dst.extend_from_slice(e.foreground_cmd.as_bytes());
        dst.put_u16(e.cwd.len() as u16);
        dst.extend_from_slice(e.cwd.as_bytes());
        dst.put_u16(e.client_name.len() as u16);
        dst.extend_from_slice(e.client_name.as_bytes());
    }
}

/// Compute the encoded byte length of a single SessionEntry (without the entry_len prefix).
fn entry_encoded_len(e: &SessionEntry) -> usize {
    4 // id: u32
    + 2 + e.name.len()
    + 2 + e.pty_path.len()
    + 4 // shell_pid
    + 8 // created_at
    + 1 // attached
    + 8 // last_heartbeat
    + 2 + e.foreground_cmd.len()
    + 2 + e.cwd.len()
    + 2 + e.client_name.len()
}

fn decode_string(payload: BytesMut) -> Result<String, io::Error> {
    String::from_utf8(payload.to_vec()).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn expect_min_len(payload: &BytesMut, min: usize, name: &str) -> Result<(), io::Error> {
    if payload.len() < min {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{name} frame must be at least {min} bytes"),
        ));
    }
    Ok(())
}

fn read_u16(payload: &[u8], offset: usize) -> u16 {
    u16::from_be_bytes([payload[offset], payload[offset + 1]])
}

fn read_u32(payload: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes([
        payload[offset],
        payload[offset + 1],
        payload[offset + 2],
        payload[offset + 3],
    ])
}

fn read_i32(payload: &[u8], offset: usize) -> i32 {
    i32::from_be_bytes([
        payload[offset],
        payload[offset + 1],
        payload[offset + 2],
        payload[offset + 3],
    ])
}

fn read_u64(payload: &[u8], offset: usize) -> u64 {
    u64::from_be_bytes([
        payload[offset],
        payload[offset + 1],
        payload[offset + 2],
        payload[offset + 3],
        payload[offset + 4],
        payload[offset + 5],
        payload[offset + 6],
        payload[offset + 7],
    ])
}

/// Auto-offset-tracking reader for decoding fixed-field payloads.
struct PayloadReader<'a> {
    data: &'a [u8],
    off: usize,
}

impl<'a> PayloadReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, off: 0 }
    }

    fn u16(&mut self) -> u16 {
        let v = read_u16(self.data, self.off);
        self.off += 2;
        v
    }

    fn u32(&mut self) -> u32 {
        let v = read_u32(self.data, self.off);
        self.off += 4;
        v
    }

    fn i32(&mut self) -> i32 {
        let v = read_i32(self.data, self.off);
        self.off += 4;
        v
    }

    fn u64(&mut self) -> u64 {
        let v = read_u64(self.data, self.off);
        self.off += 8;
        v
    }

    fn offset(&self) -> usize {
        self.off
    }
}

/// Encode a fixed-field frame: writes type byte, auto-computes payload length, writes fields.
macro_rules! encode_fields {
    ($dst:expr, $ty:expr $(, $val:expr => $method:ident)*) => {{
        let payload_len: u32 = 0 $(+ encode_fields!(@size $method))*;
        $dst.put_u8($ty);
        $dst.put_u32(payload_len);
        $($dst.$method($val);)*
    }};
    (@size put_u8) => { 1 };
    (@size put_u16) => { 2 };
    (@size put_u32) => { 4 };
    (@size put_i32) => { 4 };
    (@size put_u64) => { 8 };
}

fn decode_env(payload: BytesMut) -> Result<Option<Frame>, io::Error> {
    let p = &payload[..];
    if p.len() < 4 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "env frame too short"));
    }
    let count = read_u32(p, 0) as usize;
    let mut off = 4;
    let mut vars = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        if off + 2 > p.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "env frame truncated"));
        }
        let klen = read_u16(p, off) as usize;
        off += 2;
        if off + klen + 2 > p.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "env frame truncated"));
        }
        let key = String::from_utf8(p[off..off + klen].to_vec())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        off += klen;
        let vlen = read_u16(p, off) as usize;
        off += 2;
        if off + vlen > p.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "env frame truncated"));
        }
        let val = String::from_utf8(p[off..off + vlen].to_vec())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        off += vlen;
        vars.push((key, val));
    }
    Ok(Some(Frame::Env { vars }))
}

fn decode_session_info(payload: BytesMut) -> Result<Option<Frame>, io::Error> {
    let p = &payload[..];
    if p.len() < 4 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "session info frame too short"));
    }
    let count = read_u32(p, 0) as usize;
    let mut off = 4;
    let mut sessions = Vec::with_capacity(count.min(1024));
    let read_str = |p: &[u8], off: &mut usize| -> Result<String, io::Error> {
        if *off + 2 > p.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "session info truncated"));
        }
        let len = read_u16(p, *off) as usize;
        *off += 2;
        if *off + len > p.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "session info truncated"));
        }
        let s = String::from_utf8(p[*off..*off + len].to_vec())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        *off += len;
        Ok(s)
    };
    for _ in 0..count {
        // Read entry_len prefix
        if off + 4 > p.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "session info truncated"));
        }
        let entry_len = read_u32(p, off) as usize;
        off += 4;
        if off + entry_len > p.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "session info truncated"));
        }
        let entry_end = off + entry_len;
        let entry_slice = &p[off..entry_end];

        // Decode within entry_slice using a local offset
        let mut eoff = 0usize;
        if entry_slice.len() < 4 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "session entry too short"));
        }
        let id = read_u32(entry_slice, eoff);
        eoff += 4;

        let read_entry_str = |s: &[u8], eoff: &mut usize| -> Result<String, io::Error> {
            if *eoff + 2 > s.len() {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "session entry truncated"));
            }
            let len = read_u16(s, *eoff) as usize;
            *eoff += 2;
            if *eoff + len > s.len() {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "session entry truncated"));
            }
            let val = String::from_utf8(s[*eoff..*eoff + len].to_vec())
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            *eoff += len;
            Ok(val)
        };

        let name = read_entry_str(entry_slice, &mut eoff)?;
        let pty_path = read_entry_str(entry_slice, &mut eoff)?;
        // Fixed fields: shell_pid(4) + created_at(8) + attached(1) + last_heartbeat(8) = 21
        if eoff + 21 > entry_slice.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "session entry truncated"));
        }
        let shell_pid = read_u32(entry_slice, eoff);
        eoff += 4;
        let created_at = read_u64(entry_slice, eoff);
        eoff += 8;
        let attached = entry_slice[eoff] != 0;
        eoff += 1;
        let last_heartbeat = read_u64(entry_slice, eoff);
        eoff += 8;
        let foreground_cmd = read_entry_str(entry_slice, &mut eoff)?;
        let cwd = read_entry_str(entry_slice, &mut eoff)?;
        let client_name = read_entry_str(entry_slice, &mut eoff)?;
        // Skip any unknown trailing bytes in this entry
        sessions.push(SessionEntry {
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
        });
        off = entry_end;
    }
    // Suppress unused-closure warning: read_str is kept for reference but entry decoding
    // uses read_entry_str within the entry_slice bounds.
    let _ = &read_str;
    Ok(Some(Frame::SessionInfo { sessions }))
}

impl Decoder for FrameCodec {
    type Item = Frame;
    type Error = io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Frame>, io::Error> {
        if src.len() < HEADER_LEN {
            return Ok(None);
        }

        let frame_type = src[0];
        let payload_len = u32::from_be_bytes([src[1], src[2], src[3], src[4]]) as usize;

        if payload_len > MAX_FRAME_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("frame payload too large: {payload_len} bytes (max {MAX_FRAME_SIZE})"),
            ));
        }

        if src.len() < HEADER_LEN + payload_len {
            src.reserve(HEADER_LEN + payload_len - src.len());
            return Ok(None);
        }

        src.advance(HEADER_LEN);
        let payload = src.split_to(payload_len);

        match frame_type {
            // Blob frames
            TYPE_DATA => Ok(Some(Frame::Data(payload.freeze()))),
            TYPE_CLIPBOARD_SET => Ok(Some(Frame::ClipboardSet { data: payload.freeze() })),
            TYPE_CLIPBOARD_DATA => Ok(Some(Frame::ClipboardData { data: payload.freeze() })),

            // Fixed-field frames (PayloadReader auto-tracks offsets)
            TYPE_RESIZE => {
                expect_min_len(&payload, 4, "resize")?;
                let mut r = PayloadReader::new(&payload);
                Ok(Some(Frame::Resize { cols: r.u16(), rows: r.u16() }))
            }
            TYPE_EXIT => {
                expect_min_len(&payload, 4, "exit")?;
                Ok(Some(Frame::Exit { code: PayloadReader::new(&payload).i32() }))
            }
            TYPE_HELLO => {
                expect_min_len(&payload, 6, "hello")?;
                let mut r = PayloadReader::new(&payload);
                Ok(Some(Frame::Hello { version: r.u16(), capabilities: r.u32() }))
            }
            TYPE_HELLO_ACK => {
                expect_min_len(&payload, 6, "hello ack")?;
                let mut r = PayloadReader::new(&payload);
                Ok(Some(Frame::HelloAck { version: r.u16(), capabilities: r.u32() }))
            }
            TYPE_AGENT_OPEN => {
                expect_min_len(&payload, 4, "agent open")?;
                Ok(Some(Frame::AgentOpen { channel_id: PayloadReader::new(&payload).u32() }))
            }
            TYPE_AGENT_CLOSE => {
                expect_min_len(&payload, 4, "agent close")?;
                Ok(Some(Frame::AgentClose { channel_id: PayloadReader::new(&payload).u32() }))
            }
            TYPE_TUNNEL_LISTEN => {
                expect_min_len(&payload, 2, "tunnel listen")?;
                Ok(Some(Frame::TunnelListen { port: PayloadReader::new(&payload).u16() }))
            }
            TYPE_TUNNEL_OPEN => {
                expect_min_len(&payload, 4, "tunnel open")?;
                Ok(Some(Frame::TunnelOpen { channel_id: PayloadReader::new(&payload).u32() }))
            }
            TYPE_TUNNEL_CLOSE => {
                expect_min_len(&payload, 4, "tunnel close")?;
                Ok(Some(Frame::TunnelClose { channel_id: PayloadReader::new(&payload).u32() }))
            }
            TYPE_SEND_OFFER => {
                expect_min_len(&payload, 12, "send offer")?;
                let mut r = PayloadReader::new(&payload);
                Ok(Some(Frame::SendOffer { file_count: r.u32(), total_bytes: r.u64() }))
            }
            TYPE_PORT_FORWARD_LISTEN => {
                expect_min_len(&payload, 8, "port forward listen")?;
                let mut r = PayloadReader::new(&payload);
                Ok(Some(Frame::PortForwardListen {
                    forward_id: r.u32(),
                    listen_port: r.u16(),
                    target_port: r.u16(),
                }))
            }
            TYPE_PORT_FORWARD_READY => {
                expect_min_len(&payload, 4, "port forward ready")?;
                Ok(Some(Frame::PortForwardReady { forward_id: PayloadReader::new(&payload).u32() }))
            }
            TYPE_PORT_FORWARD_OPEN => {
                expect_min_len(&payload, 10, "port forward open")?;
                let mut r = PayloadReader::new(&payload);
                Ok(Some(Frame::PortForwardOpen {
                    forward_id: r.u32(),
                    channel_id: r.u32(),
                    target_port: r.u16(),
                }))
            }
            TYPE_PORT_FORWARD_CLOSE => {
                expect_min_len(&payload, 4, "port forward close")?;
                Ok(Some(Frame::PortForwardClose { channel_id: PayloadReader::new(&payload).u32() }))
            }
            TYPE_PORT_FORWARD_STOP => {
                expect_min_len(&payload, 4, "port forward stop")?;
                Ok(Some(Frame::PortForwardStop { forward_id: PayloadReader::new(&payload).u32() }))
            }
            TYPE_SESSION_CREATED => {
                expect_min_len(&payload, 4, "session created")?;
                Ok(Some(Frame::SessionCreated { id: PayloadReader::new(&payload).u32() }))
            }

            // Prefix + blob frames (fixed header, trailing bytes)
            TYPE_AGENT_DATA => {
                expect_min_len(&payload, 4, "agent data")?;
                let mut r = PayloadReader::new(&payload);
                let channel_id = r.u32();
                let off = r.offset();
                Ok(Some(Frame::AgentData { channel_id, data: payload.freeze().slice(off..) }))
            }
            TYPE_TUNNEL_DATA => {
                expect_min_len(&payload, 4, "tunnel data")?;
                let mut r = PayloadReader::new(&payload);
                let channel_id = r.u32();
                let off = r.offset();
                Ok(Some(Frame::TunnelData { channel_id, data: payload.freeze().slice(off..) }))
            }
            TYPE_PORT_FORWARD_DATA => {
                expect_min_len(&payload, 4, "port forward data")?;
                let mut r = PayloadReader::new(&payload);
                let channel_id = r.u32();
                let off = r.offset();
                Ok(Some(Frame::PortForwardData { channel_id, data: payload.freeze().slice(off..) }))
            }

            // Empty frames
            TYPE_DETACHED => Ok(Some(Frame::Detached)),
            TYPE_PING => Ok(Some(Frame::Ping)),
            TYPE_PONG => Ok(Some(Frame::Pong)),
            TYPE_AGENT_FORWARD => Ok(Some(Frame::AgentForward)),
            TYPE_OPEN_FORWARD => Ok(Some(Frame::OpenForward)),
            TYPE_CLIPBOARD_GET => Ok(Some(Frame::ClipboardGet)),
            TYPE_SEND_DONE => Ok(Some(Frame::SendDone)),
            TYPE_LIST_SESSIONS => Ok(Some(Frame::ListSessions)),
            TYPE_KILL_SERVER => Ok(Some(Frame::KillServer)),
            TYPE_OK => Ok(Some(Frame::Ok)),

            // String frames
            TYPE_OPEN_URL => Ok(Some(Frame::OpenUrl { url: decode_string(payload)? })),
            TYPE_SEND_CANCEL => Ok(Some(Frame::SendCancel { reason: decode_string(payload)? })),
            TYPE_TAIL => Ok(Some(Frame::Tail { session: decode_string(payload)? })),
            TYPE_KILL_SESSION => Ok(Some(Frame::KillSession { session: decode_string(payload)? })),
            TYPE_SEND_FILE => {
                let session = String::from_utf8(payload.to_vec())
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                Ok(Some(Frame::SendFile { session }))
            }

            // Structured frames
            TYPE_NEW_SESSION => {
                expect_min_len(&payload, 12, "new session")?;
                let p = &payload[..];
                let mut off = 0usize;
                let name_len = read_u16(p, off) as usize;
                off += 2;
                if off + name_len + 2 > p.len() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "new session frame truncated",
                    ));
                }
                let name = String::from_utf8(p[off..off + name_len].to_vec())
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                off += name_len;
                let cmd_len = read_u16(p, off) as usize;
                off += 2;
                if off + cmd_len + 2 > p.len() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "new session frame truncated",
                    ));
                }
                let command = String::from_utf8(p[off..off + cmd_len].to_vec())
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                off += cmd_len;
                let cwd_len = read_u16(p, off) as usize;
                off += 2;
                if off + cwd_len + 4 > p.len() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "new session frame truncated",
                    ));
                }
                let cwd = String::from_utf8(p[off..off + cwd_len].to_vec())
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                off += cwd_len;
                let cols = read_u16(p, off);
                off += 2;
                let rows = read_u16(p, off);
                off += 2;
                let client_name = if off + 2 <= p.len() {
                    let cn_len = read_u16(p, off) as usize;
                    off += 2;
                    if off + cn_len > p.len() {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "new session frame truncated",
                        ));
                    }
                    String::from_utf8(p[off..off + cn_len].to_vec())
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?
                } else {
                    String::new()
                };
                Ok(Some(Frame::NewSession { name, command, cwd, cols, rows, client_name }))
            }
            TYPE_ATTACH => {
                expect_min_len(&payload, 5, "attach")?;
                let p = &payload[..];
                let mut off = 0usize;
                let session_len = read_u16(p, off) as usize;
                off += 2;
                if off + session_len + 2 > p.len() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "attach frame truncated",
                    ));
                }
                let session = String::from_utf8(p[off..off + session_len].to_vec())
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                off += session_len;
                let cn_len = read_u16(p, off) as usize;
                off += 2;
                if off + cn_len + 1 > p.len() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "attach frame truncated",
                    ));
                }
                let client_name = String::from_utf8(p[off..off + cn_len].to_vec())
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                off += cn_len;
                let force = p[off] != 0;
                Ok(Some(Frame::Attach { session, client_name, force }))
            }
            TYPE_RENAME_SESSION => {
                if payload.len() < 2 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "rename session frame too short",
                    ));
                }
                let session_len = read_u16(&payload, 0) as usize;
                if 2 + session_len > payload.len() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "rename session frame truncated",
                    ));
                }
                let session = String::from_utf8(payload[2..2 + session_len].to_vec())
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                let new_name = String::from_utf8(payload[2 + session_len..].to_vec())
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                Ok(Some(Frame::RenameSession { session, new_name }))
            }
            TYPE_ERROR => {
                expect_min_len(&payload, 2, "error")?;
                let code = ErrorCode::from_u16(read_u16(&payload, 0));
                let message = String::from_utf8(payload[2..].to_vec())
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                Ok(Some(Frame::Error { code, message }))
            }

            // Custom frames
            TYPE_ENV => decode_env(payload),
            TYPE_SESSION_INFO => decode_session_info(payload),

            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown frame type: 0x{frame_type:02x}"),
            )),
        }
    }
}

impl Encoder<Frame> for FrameCodec {
    type Error = io::Error;

    fn encode(&mut self, frame: Frame, dst: &mut BytesMut) -> Result<(), io::Error> {
        match frame {
            // Blob frames
            Frame::Data(data) => encode_blob(dst, TYPE_DATA, &data),
            Frame::ClipboardSet { data } => encode_blob(dst, TYPE_CLIPBOARD_SET, &data),
            Frame::ClipboardData { data } => encode_blob(dst, TYPE_CLIPBOARD_DATA, &data),

            // Fixed-field frames (encode_fields! auto-computes payload length)
            Frame::Resize { cols, rows } => {
                encode_fields!(dst, TYPE_RESIZE, cols => put_u16, rows => put_u16);
            }
            Frame::Exit { code } => {
                encode_fields!(dst, TYPE_EXIT, code => put_i32);
            }
            Frame::Hello { version, capabilities } => {
                encode_fields!(dst, TYPE_HELLO, version => put_u16, capabilities => put_u32);
            }
            Frame::HelloAck { version, capabilities } => {
                encode_fields!(dst, TYPE_HELLO_ACK, version => put_u16, capabilities => put_u32);
            }
            Frame::AgentOpen { channel_id } => {
                encode_fields!(dst, TYPE_AGENT_OPEN, channel_id => put_u32);
            }
            Frame::AgentClose { channel_id } => {
                encode_fields!(dst, TYPE_AGENT_CLOSE, channel_id => put_u32);
            }
            Frame::TunnelListen { port } => {
                encode_fields!(dst, TYPE_TUNNEL_LISTEN, port => put_u16);
            }
            Frame::TunnelOpen { channel_id } => {
                encode_fields!(dst, TYPE_TUNNEL_OPEN, channel_id => put_u32);
            }
            Frame::TunnelClose { channel_id } => {
                encode_fields!(dst, TYPE_TUNNEL_CLOSE, channel_id => put_u32);
            }
            Frame::SendOffer { file_count, total_bytes } => {
                encode_fields!(dst, TYPE_SEND_OFFER, file_count => put_u32, total_bytes => put_u64);
            }
            Frame::PortForwardListen { forward_id, listen_port, target_port } => {
                encode_fields!(dst, TYPE_PORT_FORWARD_LISTEN,
                    forward_id => put_u32, listen_port => put_u16, target_port => put_u16);
            }
            Frame::PortForwardReady { forward_id } => {
                encode_fields!(dst, TYPE_PORT_FORWARD_READY, forward_id => put_u32);
            }
            Frame::PortForwardOpen { forward_id, channel_id, target_port } => {
                encode_fields!(dst, TYPE_PORT_FORWARD_OPEN,
                    forward_id => put_u32, channel_id => put_u32, target_port => put_u16);
            }
            Frame::PortForwardClose { channel_id } => {
                encode_fields!(dst, TYPE_PORT_FORWARD_CLOSE, channel_id => put_u32);
            }
            Frame::PortForwardStop { forward_id } => {
                encode_fields!(dst, TYPE_PORT_FORWARD_STOP, forward_id => put_u32);
            }
            Frame::SessionCreated { id } => {
                encode_fields!(dst, TYPE_SESSION_CREATED, id => put_u32);
            }

            // Prefix + blob frames
            Frame::AgentData { channel_id, data } => {
                encode_prefix_blob(dst, TYPE_AGENT_DATA, channel_id, &data);
            }
            Frame::TunnelData { channel_id, data } => {
                encode_prefix_blob(dst, TYPE_TUNNEL_DATA, channel_id, &data);
            }
            Frame::PortForwardData { channel_id, data } => {
                encode_prefix_blob(dst, TYPE_PORT_FORWARD_DATA, channel_id, &data);
            }

            // Empty frames
            Frame::Detached => encode_empty(dst, TYPE_DETACHED),
            Frame::Ping => encode_empty(dst, TYPE_PING),
            Frame::Pong => encode_empty(dst, TYPE_PONG),
            Frame::AgentForward => encode_empty(dst, TYPE_AGENT_FORWARD),
            Frame::OpenForward => encode_empty(dst, TYPE_OPEN_FORWARD),
            Frame::ClipboardGet => encode_empty(dst, TYPE_CLIPBOARD_GET),
            Frame::SendDone => encode_empty(dst, TYPE_SEND_DONE),
            Frame::ListSessions => encode_empty(dst, TYPE_LIST_SESSIONS),
            Frame::KillServer => encode_empty(dst, TYPE_KILL_SERVER),
            Frame::Ok => encode_empty(dst, TYPE_OK),

            // String frames
            Frame::OpenUrl { url } => encode_str(dst, TYPE_OPEN_URL, &url),
            Frame::SendCancel { reason } => encode_str(dst, TYPE_SEND_CANCEL, &reason),
            Frame::Tail { session } => encode_str(dst, TYPE_TAIL, &session),
            Frame::KillSession { session } => encode_str(dst, TYPE_KILL_SESSION, &session),
            Frame::SendFile { session } => encode_str(dst, TYPE_SEND_FILE, &session),
            Frame::RenameSession { session, new_name } => {
                let session_bytes = session.as_bytes();
                let name_bytes = new_name.as_bytes();
                let payload_len = 2 + session_bytes.len() + name_bytes.len();
                dst.put_u8(TYPE_RENAME_SESSION);
                dst.put_u32(payload_len as u32);
                dst.put_u16(session_bytes.len() as u16);
                dst.extend_from_slice(session_bytes);
                dst.extend_from_slice(name_bytes);
            }

            // Structured frames
            Frame::NewSession { name, command, cwd, cols, rows, client_name } => {
                let nb = name.as_bytes();
                let cb = command.as_bytes();
                let cwdb = cwd.as_bytes();
                let cnb = client_name.as_bytes();
                let payload_len =
                    2 + nb.len() + 2 + cb.len() + 2 + cwdb.len() + 2 + 2 + 2 + cnb.len();
                dst.put_u8(TYPE_NEW_SESSION);
                dst.put_u32(payload_len as u32);
                dst.put_u16(nb.len() as u16);
                dst.extend_from_slice(nb);
                dst.put_u16(cb.len() as u16);
                dst.extend_from_slice(cb);
                dst.put_u16(cwdb.len() as u16);
                dst.extend_from_slice(cwdb);
                dst.put_u16(cols);
                dst.put_u16(rows);
                dst.put_u16(cnb.len() as u16);
                dst.extend_from_slice(cnb);
            }
            Frame::Attach { session, client_name, force } => {
                let sb = session.as_bytes();
                let cnb = client_name.as_bytes();
                let payload_len = 2 + sb.len() + 2 + cnb.len() + 1;
                dst.put_u8(TYPE_ATTACH);
                dst.put_u32(payload_len as u32);
                dst.put_u16(sb.len() as u16);
                dst.extend_from_slice(sb);
                dst.put_u16(cnb.len() as u16);
                dst.extend_from_slice(cnb);
                dst.put_u8(if force { 1 } else { 0 });
            }
            Frame::Error { code, message } => {
                let mb = message.as_bytes();
                let payload_len = 2 + mb.len();
                dst.put_u8(TYPE_ERROR);
                dst.put_u32(payload_len as u32);
                dst.put_u16(code.to_u16());
                dst.extend_from_slice(mb);
            }

            // Custom frames
            Frame::Env { vars } => encode_env(dst, &vars),
            Frame::SessionInfo { sessions } => encode_session_info(dst, &sessions),
        }
        Ok(())
    }
}
