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

const HEADER_LEN: usize = 5; // type(1) + length(4)
const MAX_FRAME_SIZE: usize = 1 << 20; // 1 MB

/// Protocol version for handshake negotiation.
pub const PROTOCOL_VERSION: u16 = 1;

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
    /// Client signals the tunnel connection has been accepted (client → server).
    TunnelOpen,
    /// Tunnel data relay (bidirectional).
    TunnelData(Bytes),
    /// Tunnel connection closed (bidirectional).
    TunnelClose,
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
            TYPE_DATA => Ok(Some(Frame::Data(payload.freeze()))),
            TYPE_RESIZE => {
                expect_len(&payload, 4, "resize")?;
                Ok(Some(Frame::Resize { cols: read_u16(&payload, 0), rows: read_u16(&payload, 2) }))
            }
            TYPE_EXIT => {
                expect_len(&payload, 4, "exit")?;
                Ok(Some(Frame::Exit { code: read_i32(&payload, 0) }))
            }
            TYPE_DETACHED => Ok(Some(Frame::Detached)),
            TYPE_PING => Ok(Some(Frame::Ping)),
            TYPE_PONG => Ok(Some(Frame::Pong)),
            TYPE_ENV => {
                let text = String::from_utf8(payload.to_vec())
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                let vars = if text.is_empty() {
                    Vec::new()
                } else {
                    text.lines()
                        .filter_map(|line| {
                            let (k, v) = line.split_once('=')?;
                            Some((k.to_string(), v.to_string()))
                        })
                        .collect()
                };
                Ok(Some(Frame::Env { vars }))
            }
            TYPE_AGENT_FORWARD => Ok(Some(Frame::AgentForward)),
            TYPE_AGENT_OPEN => {
                expect_len(&payload, 4, "agent open")?;
                Ok(Some(Frame::AgentOpen { channel_id: read_u32(&payload, 0) }))
            }
            TYPE_AGENT_DATA => {
                expect_min_len(&payload, 4, "agent data")?;
                let channel_id = read_u32(&payload, 0);
                let data = payload.freeze().slice(4..);
                Ok(Some(Frame::AgentData { channel_id, data }))
            }
            TYPE_AGENT_CLOSE => {
                expect_len(&payload, 4, "agent close")?;
                Ok(Some(Frame::AgentClose { channel_id: read_u32(&payload, 0) }))
            }
            TYPE_OPEN_FORWARD => Ok(Some(Frame::OpenForward)),
            TYPE_OPEN_URL => Ok(Some(Frame::OpenUrl { url: decode_string(payload)? })),
            TYPE_TUNNEL_LISTEN => {
                expect_len(&payload, 2, "tunnel listen")?;
                Ok(Some(Frame::TunnelListen { port: read_u16(&payload, 0) }))
            }
            TYPE_TUNNEL_OPEN => Ok(Some(Frame::TunnelOpen)),
            TYPE_TUNNEL_DATA => Ok(Some(Frame::TunnelData(payload.freeze()))),
            TYPE_TUNNEL_CLOSE => Ok(Some(Frame::TunnelClose)),
            TYPE_SEND_OFFER => {
                expect_len(&payload, 12, "send offer")?;
                Ok(Some(Frame::SendOffer {
                    file_count: read_u32(&payload, 0),
                    total_bytes: read_u64(&payload, 4),
                }))
            }
            TYPE_SEND_DONE => Ok(Some(Frame::SendDone)),
            TYPE_SEND_CANCEL => Ok(Some(Frame::SendCancel { reason: decode_string(payload)? })),
            TYPE_PORT_FORWARD_LISTEN => {
                expect_len(&payload, 8, "port forward listen")?;
                Ok(Some(Frame::PortForwardListen {
                    forward_id: read_u32(&payload, 0),
                    listen_port: read_u16(&payload, 4),
                    target_port: read_u16(&payload, 6),
                }))
            }
            TYPE_PORT_FORWARD_READY => {
                expect_len(&payload, 4, "port forward ready")?;
                Ok(Some(Frame::PortForwardReady { forward_id: read_u32(&payload, 0) }))
            }
            TYPE_PORT_FORWARD_OPEN => {
                expect_len(&payload, 10, "port forward open")?;
                Ok(Some(Frame::PortForwardOpen {
                    forward_id: read_u32(&payload, 0),
                    channel_id: read_u32(&payload, 4),
                    target_port: read_u16(&payload, 8),
                }))
            }
            TYPE_PORT_FORWARD_DATA => {
                expect_min_len(&payload, 4, "port forward data")?;
                let channel_id = read_u32(&payload, 0);
                let data = payload.freeze().slice(4..);
                Ok(Some(Frame::PortForwardData { channel_id, data }))
            }
            TYPE_PORT_FORWARD_CLOSE => {
                expect_len(&payload, 4, "port forward close")?;
                Ok(Some(Frame::PortForwardClose { channel_id: read_u32(&payload, 0) }))
            }
            TYPE_PORT_FORWARD_STOP => {
                expect_len(&payload, 4, "port forward stop")?;
                Ok(Some(Frame::PortForwardStop { forward_id: read_u32(&payload, 0) }))
            }
            TYPE_SEND_FILE => {
                expect_min_len(&payload, 1, "send file")?;
                let role = payload[payload.len() - 1];
                let session = String::from_utf8(payload[..payload.len() - 1].to_vec())
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                Ok(Some(Frame::SendFile { session, role }))
            }
            TYPE_HELLO => {
                expect_len(&payload, 2, "hello")?;
                Ok(Some(Frame::Hello { version: read_u16(&payload, 0) }))
            }
            TYPE_HELLO_ACK => {
                expect_len(&payload, 2, "hello ack")?;
                Ok(Some(Frame::HelloAck { version: read_u16(&payload, 0) }))
            }
            TYPE_NEW_SESSION => Ok(Some(Frame::NewSession { name: decode_string(payload)? })),
            TYPE_ATTACH => Ok(Some(Frame::Attach { session: decode_string(payload)? })),
            TYPE_TAIL => Ok(Some(Frame::Tail { session: decode_string(payload)? })),
            TYPE_LIST_SESSIONS => Ok(Some(Frame::ListSessions)),
            TYPE_KILL_SESSION => Ok(Some(Frame::KillSession { session: decode_string(payload)? })),
            TYPE_KILL_SERVER => Ok(Some(Frame::KillServer)),
            TYPE_SESSION_CREATED => Ok(Some(Frame::SessionCreated { id: decode_string(payload)? })),
            TYPE_SESSION_INFO => {
                let text = String::from_utf8(payload.to_vec())
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                let sessions = if text.is_empty() {
                    Vec::new()
                } else {
                    text.lines()
                        .filter_map(|line| {
                            let parts: Vec<&str> = line.split('\t').collect();
                            if parts.len() == 7 {
                                Some(SessionEntry {
                                    id: parts[0].to_string(),
                                    name: parts[1].to_string(),
                                    pty_path: parts[2].to_string(),
                                    shell_pid: parts[3].parse().unwrap_or(0),
                                    created_at: parts[4].parse().unwrap_or(0),
                                    attached: parts[5] == "1",
                                    last_heartbeat: parts[6].parse().unwrap_or(0),
                                })
                            } else {
                                None
                            }
                        })
                        .collect()
                };
                Ok(Some(Frame::SessionInfo { sessions }))
            }
            TYPE_OK => Ok(Some(Frame::Ok)),
            TYPE_ERROR => Ok(Some(Frame::Error { message: decode_string(payload)? })),
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
            Frame::Data(data) => {
                dst.put_u8(TYPE_DATA);
                dst.put_u32(data.len() as u32);
                dst.extend_from_slice(&data);
            }
            Frame::Resize { cols, rows } => {
                dst.put_u8(TYPE_RESIZE);
                dst.put_u32(4);
                dst.put_u16(cols);
                dst.put_u16(rows);
            }
            Frame::Exit { code } => {
                dst.put_u8(TYPE_EXIT);
                dst.put_u32(4);
                dst.put_i32(code);
            }
            Frame::Detached => encode_empty(dst, TYPE_DETACHED),
            Frame::Ping => encode_empty(dst, TYPE_PING),
            Frame::Pong => encode_empty(dst, TYPE_PONG),
            Frame::Env { vars } => {
                // Strip newlines from keys/values to prevent injection of extra
                // key=value pairs via the newline-delimited wire format.
                let text: String = vars
                    .iter()
                    .map(|(k, v)| {
                        let k = k.replace('\n', "");
                        let v = v.replace('\n', "");
                        format!("{k}={v}")
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                dst.put_u8(TYPE_ENV);
                dst.put_u32(text.len() as u32);
                dst.extend_from_slice(text.as_bytes());
            }
            Frame::AgentForward => encode_empty(dst, TYPE_AGENT_FORWARD),
            Frame::AgentOpen { channel_id } => {
                dst.put_u8(TYPE_AGENT_OPEN);
                dst.put_u32(4);
                dst.put_u32(channel_id);
            }
            Frame::AgentData { channel_id, data } => {
                dst.put_u8(TYPE_AGENT_DATA);
                dst.put_u32(4 + data.len() as u32);
                dst.put_u32(channel_id);
                dst.extend_from_slice(&data);
            }
            Frame::AgentClose { channel_id } => {
                dst.put_u8(TYPE_AGENT_CLOSE);
                dst.put_u32(4);
                dst.put_u32(channel_id);
            }
            Frame::OpenForward => encode_empty(dst, TYPE_OPEN_FORWARD),
            Frame::OpenUrl { url } => encode_str(dst, TYPE_OPEN_URL, &url),
            Frame::TunnelListen { port } => {
                dst.put_u8(TYPE_TUNNEL_LISTEN);
                dst.put_u32(2);
                dst.put_u16(port);
            }
            Frame::TunnelOpen => encode_empty(dst, TYPE_TUNNEL_OPEN),
            Frame::TunnelData(data) => {
                dst.put_u8(TYPE_TUNNEL_DATA);
                dst.put_u32(data.len() as u32);
                dst.extend_from_slice(&data);
            }
            Frame::TunnelClose => encode_empty(dst, TYPE_TUNNEL_CLOSE),
            Frame::SendOffer { file_count, total_bytes } => {
                dst.put_u8(TYPE_SEND_OFFER);
                dst.put_u32(12); // u32 + u64
                dst.put_u32(file_count);
                dst.put_u64(total_bytes);
            }
            Frame::SendDone => encode_empty(dst, TYPE_SEND_DONE),
            Frame::SendCancel { reason } => encode_str(dst, TYPE_SEND_CANCEL, &reason),
            Frame::PortForwardListen { forward_id, listen_port, target_port } => {
                dst.put_u8(TYPE_PORT_FORWARD_LISTEN);
                dst.put_u32(8); // u32 + u16 + u16
                dst.put_u32(forward_id);
                dst.put_u16(listen_port);
                dst.put_u16(target_port);
            }
            Frame::PortForwardReady { forward_id } => {
                dst.put_u8(TYPE_PORT_FORWARD_READY);
                dst.put_u32(4);
                dst.put_u32(forward_id);
            }
            Frame::PortForwardOpen { forward_id, channel_id, target_port } => {
                dst.put_u8(TYPE_PORT_FORWARD_OPEN);
                dst.put_u32(10); // u32 + u32 + u16
                dst.put_u32(forward_id);
                dst.put_u32(channel_id);
                dst.put_u16(target_port);
            }
            Frame::PortForwardData { channel_id, data } => {
                dst.put_u8(TYPE_PORT_FORWARD_DATA);
                dst.put_u32(4 + data.len() as u32);
                dst.put_u32(channel_id);
                dst.extend_from_slice(&data);
            }
            Frame::PortForwardClose { channel_id } => {
                dst.put_u8(TYPE_PORT_FORWARD_CLOSE);
                dst.put_u32(4);
                dst.put_u32(channel_id);
            }
            Frame::PortForwardStop { forward_id } => {
                dst.put_u8(TYPE_PORT_FORWARD_STOP);
                dst.put_u32(4);
                dst.put_u32(forward_id);
            }
            Frame::SendFile { session, role } => {
                let slen = session.len();
                dst.put_u8(TYPE_SEND_FILE);
                dst.put_u32((slen + 1) as u32); // session bytes + 1 byte role
                dst.extend_from_slice(session.as_bytes());
                dst.put_u8(role);
            }
            Frame::Hello { version } => {
                dst.put_u8(TYPE_HELLO);
                dst.put_u32(2);
                dst.put_u16(version);
            }
            Frame::HelloAck { version } => {
                dst.put_u8(TYPE_HELLO_ACK);
                dst.put_u32(2);
                dst.put_u16(version);
            }
            Frame::NewSession { name } => encode_str(dst, TYPE_NEW_SESSION, &name),
            Frame::Attach { session } => encode_str(dst, TYPE_ATTACH, &session),
            Frame::Tail { session } => encode_str(dst, TYPE_TAIL, &session),
            Frame::ListSessions => encode_empty(dst, TYPE_LIST_SESSIONS),
            Frame::KillSession { session } => encode_str(dst, TYPE_KILL_SESSION, &session),
            Frame::KillServer => encode_empty(dst, TYPE_KILL_SERVER),
            Frame::SessionCreated { id } => encode_str(dst, TYPE_SESSION_CREATED, &id),
            Frame::SessionInfo { sessions } => {
                let text: String = sessions
                    .iter()
                    .map(|e| {
                        let safe_pty = e.pty_path.replace(['\t', '\n'], " ");
                        format!(
                            "{}\t{}\t{}\t{}\t{}\t{}\t{}",
                            e.id,
                            e.name,
                            safe_pty,
                            e.shell_pid,
                            e.created_at,
                            if e.attached { "1" } else { "0" },
                            e.last_heartbeat
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                dst.put_u8(TYPE_SESSION_INFO);
                dst.put_u32(text.len() as u32);
                dst.extend_from_slice(text.as_bytes());
            }
            Frame::Ok => encode_empty(dst, TYPE_OK),
            Frame::Error { message } => encode_str(dst, TYPE_ERROR, &message),
        }
        Ok(())
    }
}
