use bytes::{Buf, BufMut, Bytes, BytesMut};
use std::io;
use tokio_util::codec::{Decoder, Encoder};

const TYPE_DATA: u8 = 0x01;
const TYPE_RESIZE: u8 = 0x02;
const TYPE_EXIT: u8 = 0x03;
const TYPE_DETACHED: u8 = 0x04;
const TYPE_PING: u8 = 0x05;
const TYPE_PONG: u8 = 0x06;
const TYPE_ENV: u8 = 0x07;
const TYPE_AGENT_FORWARD: u8 = 0x08;
const TYPE_AGENT_OPEN: u8 = 0x09;
const TYPE_AGENT_DATA: u8 = 0x0A;
const TYPE_AGENT_CLOSE: u8 = 0x0B;
const TYPE_OPEN_FORWARD: u8 = 0x0C;
const TYPE_OPEN_URL: u8 = 0x0D;
const TYPE_TUNNEL_LISTEN: u8 = 0x0E;
const TYPE_TUNNEL_OPEN: u8 = 0x0F;
const TYPE_NEW_SESSION: u8 = 0x10;
const TYPE_ATTACH: u8 = 0x11;
const TYPE_LIST_SESSIONS: u8 = 0x12;
const TYPE_KILL_SESSION: u8 = 0x13;
const TYPE_KILL_SERVER: u8 = 0x14;
const TYPE_TAIL: u8 = 0x15;
const TYPE_HELLO: u8 = 0x16;
const TYPE_TUNNEL_DATA: u8 = 0x17;
const TYPE_TUNNEL_CLOSE: u8 = 0x18;
const TYPE_SEND_OFFER: u8 = 0x19;
const TYPE_SEND_DONE: u8 = 0x1A;
const TYPE_SEND_CANCEL: u8 = 0x1B;
const TYPE_SESSION_CREATED: u8 = 0x20;
const TYPE_SESSION_INFO: u8 = 0x21;
const TYPE_OK: u8 = 0x22;
const TYPE_ERROR: u8 = 0x23;
const TYPE_HELLO_ACK: u8 = 0x24;
const TYPE_SEND_FILE: u8 = 0x25;
const TYPE_PORT_FORWARD_LISTEN: u8 = 0x1C;
const TYPE_PORT_FORWARD_READY: u8 = 0x1D;
const TYPE_PORT_FORWARD_OPEN: u8 = 0x1E;
const TYPE_PORT_FORWARD_DATA: u8 = 0x1F;
const TYPE_PORT_FORWARD_CLOSE: u8 = 0x26;
const TYPE_PORT_FORWARD_STOP: u8 = 0x27;
const TYPE_RENAME_SESSION: u8 = 0x28;

const HEADER_LEN: usize = 5; // type(1) + length(4)
const MAX_FRAME_SIZE: usize = 1 << 20; // 1 MB

/// Protocol version for handshake negotiation.
pub const PROTOCOL_VERSION: u16 = 3;

/// Discriminator byte for the unified per-session service socket (`svc-{id}.sock`).
/// Sent as the first byte on every connection to route to the correct handler.
#[repr(u8)]
pub enum SvcRequest {
    OpenUrl = 1,
    Send = 2,
    Receive = 3,
    PortForward = 4,
}

impl SvcRequest {
    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            1 => Some(Self::OpenUrl),
            2 => Some(Self::Send),
            3 => Some(Self::Receive),
            4 => Some(Self::PortForward),
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
    pub id: String,
    pub name: String,
    pub pty_path: String,
    pub shell_pid: u32,
    pub created_at: u64,
    pub attached: bool,
    pub last_heartbeat: u64,
    pub foreground_cmd: String,
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
    },
    /// Protocol version acknowledgement (server → client).
    HelloAck {
        version: u16,
    },
    // Control requests
    /// Local-side file transfer routing (client → daemon).
    SendFile {
        session: String,
        role: u8,
    },
    NewSession {
        name: String,
        command: String,
    },
    Attach {
        session: String,
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
        id: String,
    },
    SessionInfo {
        sessions: Vec<SessionEntry>,
    },
    Ok,
    Error {
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
    let body_len: usize = 4 + sessions
        .iter()
        .map(|e| {
            2 + e.id.len()
                + 2
                + e.name.len()
                + 2
                + e.pty_path.len()
                + 21
                + 2
                + e.foreground_cmd.len()
        })
        .sum::<usize>();
    dst.put_u8(TYPE_SESSION_INFO);
    dst.put_u32(body_len as u32);
    dst.put_u32(sessions.len() as u32);
    for e in sessions {
        dst.put_u16(e.id.len() as u16);
        dst.extend_from_slice(e.id.as_bytes());
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
    }
}

fn decode_string(payload: BytesMut) -> Result<String, io::Error> {
    String::from_utf8(payload.to_vec()).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn expect_len(payload: &BytesMut, expected: usize, name: &str) -> Result<(), io::Error> {
    if payload.len() != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{name} frame must be {expected} bytes"),
        ));
    }
    Ok(())
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
        let id = read_str(p, &mut off)?;
        let name = read_str(p, &mut off)?;
        let pty_path = read_str(p, &mut off)?;
        // Fixed fields: shell_pid(4) + created_at(8) + attached(1) + last_heartbeat(8) = 21
        if off + 21 > p.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "session info truncated"));
        }
        let shell_pid = read_u32(p, off);
        off += 4;
        let created_at = read_u64(p, off);
        off += 8;
        let attached = p[off] != 0;
        off += 1;
        let last_heartbeat = read_u64(p, off);
        off += 8;
        // Optional field: foreground_cmd (backwards compat -- empty if absent)
        let foreground_cmd = if off + 2 <= p.len() {
            read_str(p, &mut off).unwrap_or_default()
        } else {
            String::new()
        };
        sessions.push(SessionEntry {
            id,
            name,
            pty_path,
            shell_pid,
            created_at,
            attached,
            last_heartbeat,
            foreground_cmd,
        });
    }
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

            // Fixed-field frames (PayloadReader auto-tracks offsets)
            TYPE_RESIZE => {
                expect_len(&payload, 4, "resize")?;
                let mut r = PayloadReader::new(&payload);
                Ok(Some(Frame::Resize { cols: r.u16(), rows: r.u16() }))
            }
            TYPE_EXIT => {
                expect_len(&payload, 4, "exit")?;
                Ok(Some(Frame::Exit { code: PayloadReader::new(&payload).i32() }))
            }
            TYPE_HELLO => {
                expect_len(&payload, 2, "hello")?;
                Ok(Some(Frame::Hello { version: PayloadReader::new(&payload).u16() }))
            }
            TYPE_HELLO_ACK => {
                expect_len(&payload, 2, "hello ack")?;
                Ok(Some(Frame::HelloAck { version: PayloadReader::new(&payload).u16() }))
            }
            TYPE_AGENT_OPEN => {
                expect_len(&payload, 4, "agent open")?;
                Ok(Some(Frame::AgentOpen { channel_id: PayloadReader::new(&payload).u32() }))
            }
            TYPE_AGENT_CLOSE => {
                expect_len(&payload, 4, "agent close")?;
                Ok(Some(Frame::AgentClose { channel_id: PayloadReader::new(&payload).u32() }))
            }
            TYPE_TUNNEL_LISTEN => {
                expect_len(&payload, 2, "tunnel listen")?;
                Ok(Some(Frame::TunnelListen { port: PayloadReader::new(&payload).u16() }))
            }
            TYPE_TUNNEL_OPEN => {
                expect_len(&payload, 4, "tunnel open")?;
                Ok(Some(Frame::TunnelOpen { channel_id: PayloadReader::new(&payload).u32() }))
            }
            TYPE_TUNNEL_CLOSE => {
                expect_len(&payload, 4, "tunnel close")?;
                Ok(Some(Frame::TunnelClose { channel_id: PayloadReader::new(&payload).u32() }))
            }
            TYPE_SEND_OFFER => {
                expect_len(&payload, 12, "send offer")?;
                let mut r = PayloadReader::new(&payload);
                Ok(Some(Frame::SendOffer { file_count: r.u32(), total_bytes: r.u64() }))
            }
            TYPE_PORT_FORWARD_LISTEN => {
                expect_len(&payload, 8, "port forward listen")?;
                let mut r = PayloadReader::new(&payload);
                Ok(Some(Frame::PortForwardListen {
                    forward_id: r.u32(),
                    listen_port: r.u16(),
                    target_port: r.u16(),
                }))
            }
            TYPE_PORT_FORWARD_READY => {
                expect_len(&payload, 4, "port forward ready")?;
                Ok(Some(Frame::PortForwardReady { forward_id: PayloadReader::new(&payload).u32() }))
            }
            TYPE_PORT_FORWARD_OPEN => {
                expect_len(&payload, 10, "port forward open")?;
                let mut r = PayloadReader::new(&payload);
                Ok(Some(Frame::PortForwardOpen {
                    forward_id: r.u32(),
                    channel_id: r.u32(),
                    target_port: r.u16(),
                }))
            }
            TYPE_PORT_FORWARD_CLOSE => {
                expect_len(&payload, 4, "port forward close")?;
                Ok(Some(Frame::PortForwardClose { channel_id: PayloadReader::new(&payload).u32() }))
            }
            TYPE_PORT_FORWARD_STOP => {
                expect_len(&payload, 4, "port forward stop")?;
                Ok(Some(Frame::PortForwardStop { forward_id: PayloadReader::new(&payload).u32() }))
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
            TYPE_SEND_DONE => Ok(Some(Frame::SendDone)),
            TYPE_LIST_SESSIONS => Ok(Some(Frame::ListSessions)),
            TYPE_KILL_SERVER => Ok(Some(Frame::KillServer)),
            TYPE_OK => Ok(Some(Frame::Ok)),

            // String frames
            TYPE_OPEN_URL => Ok(Some(Frame::OpenUrl { url: decode_string(payload)? })),
            TYPE_SEND_CANCEL => Ok(Some(Frame::SendCancel { reason: decode_string(payload)? })),
            TYPE_NEW_SESSION => {
                if payload.len() < 2 {
                    return Ok(Some(Frame::NewSession {
                        name: String::new(),
                        command: String::new(),
                    }));
                }
                let name_len = read_u16(&payload, 0) as usize;
                if 2 + name_len > payload.len() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "new session frame truncated",
                    ));
                }
                let name = String::from_utf8(payload[2..2 + name_len].to_vec())
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                let command = String::from_utf8(payload[2 + name_len..].to_vec())
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                Ok(Some(Frame::NewSession { name, command }))
            }
            TYPE_ATTACH => Ok(Some(Frame::Attach { session: decode_string(payload)? })),
            TYPE_TAIL => Ok(Some(Frame::Tail { session: decode_string(payload)? })),
            TYPE_KILL_SESSION => Ok(Some(Frame::KillSession { session: decode_string(payload)? })),
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
            TYPE_SESSION_CREATED => Ok(Some(Frame::SessionCreated { id: decode_string(payload)? })),
            TYPE_ERROR => Ok(Some(Frame::Error { message: decode_string(payload)? })),

            // Custom frames
            TYPE_ENV => decode_env(payload),
            TYPE_SESSION_INFO => decode_session_info(payload),
            TYPE_SEND_FILE => {
                expect_min_len(&payload, 1, "send file")?;
                let role = payload[payload.len() - 1];
                let session = String::from_utf8(payload[..payload.len() - 1].to_vec())
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                Ok(Some(Frame::SendFile { session, role }))
            }

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

            // Fixed-field frames (encode_fields! auto-computes payload length)
            Frame::Resize { cols, rows } => {
                encode_fields!(dst, TYPE_RESIZE, cols => put_u16, rows => put_u16);
            }
            Frame::Exit { code } => {
                encode_fields!(dst, TYPE_EXIT, code => put_i32);
            }
            Frame::Hello { version } => {
                encode_fields!(dst, TYPE_HELLO, version => put_u16);
            }
            Frame::HelloAck { version } => {
                encode_fields!(dst, TYPE_HELLO_ACK, version => put_u16);
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
            Frame::SendDone => encode_empty(dst, TYPE_SEND_DONE),
            Frame::ListSessions => encode_empty(dst, TYPE_LIST_SESSIONS),
            Frame::KillServer => encode_empty(dst, TYPE_KILL_SERVER),
            Frame::Ok => encode_empty(dst, TYPE_OK),

            // String frames
            Frame::OpenUrl { url } => encode_str(dst, TYPE_OPEN_URL, &url),
            Frame::SendCancel { reason } => encode_str(dst, TYPE_SEND_CANCEL, &reason),
            Frame::NewSession { name, command } => {
                let name_bytes = name.as_bytes();
                let cmd_bytes = command.as_bytes();
                let payload_len = 2 + name_bytes.len() + cmd_bytes.len();
                dst.put_u8(TYPE_NEW_SESSION);
                dst.put_u32(payload_len as u32);
                dst.put_u16(name_bytes.len() as u16);
                dst.extend_from_slice(name_bytes);
                dst.extend_from_slice(cmd_bytes);
            }
            Frame::Attach { session } => encode_str(dst, TYPE_ATTACH, &session),
            Frame::Tail { session } => encode_str(dst, TYPE_TAIL, &session),
            Frame::KillSession { session } => encode_str(dst, TYPE_KILL_SESSION, &session),
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
            Frame::SessionCreated { id } => encode_str(dst, TYPE_SESSION_CREATED, &id),
            Frame::Error { message } => encode_str(dst, TYPE_ERROR, &message),

            // Custom frames
            Frame::Env { vars } => encode_env(dst, &vars),
            Frame::SessionInfo { sessions } => encode_session_info(dst, &sessions),
            Frame::SendFile { session, role } => {
                dst.put_u8(TYPE_SEND_FILE);
                dst.put_u32((session.len() + 1) as u32);
                dst.extend_from_slice(session.as_bytes());
                dst.put_u8(role);
            }
        }
        Ok(())
    }
}
