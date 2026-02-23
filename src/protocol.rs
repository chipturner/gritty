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
const TYPE_NEW_SESSION: u8 = 0x10;
const TYPE_ATTACH: u8 = 0x11;
const TYPE_LIST_SESSIONS: u8 = 0x12;
const TYPE_KILL_SESSION: u8 = 0x13;
const TYPE_KILL_SERVER: u8 = 0x14;
const TYPE_SESSION_CREATED: u8 = 0x20;
const TYPE_SESSION_INFO: u8 = 0x21;
const TYPE_OK: u8 = 0x22;
const TYPE_ERROR: u8 = 0x23;

const HEADER_LEN: usize = 5; // type(1) + length(4)
const MAX_FRAME_SIZE: usize = 1 << 20; // 1 MB

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
    // Control requests
    NewSession {
        name: String,
    },
    Attach {
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
                if payload.len() != 4 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "resize frame must be 4 bytes",
                    ));
                }
                let cols = u16::from_be_bytes([payload[0], payload[1]]);
                let rows = u16::from_be_bytes([payload[2], payload[3]]);
                Ok(Some(Frame::Resize { cols, rows }))
            }
            TYPE_EXIT => {
                if payload.len() != 4 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "exit frame must be 4 bytes",
                    ));
                }
                let code = i32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
                Ok(Some(Frame::Exit { code }))
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
                if payload.len() != 4 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "agent open frame must be 4 bytes",
                    ));
                }
                let channel_id =
                    u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
                Ok(Some(Frame::AgentOpen { channel_id }))
            }
            TYPE_AGENT_DATA => {
                if payload.len() < 4 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "agent data frame must be at least 4 bytes",
                    ));
                }
                let channel_id =
                    u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
                let data = payload.freeze().slice(4..);
                Ok(Some(Frame::AgentData { channel_id, data }))
            }
            TYPE_AGENT_CLOSE => {
                if payload.len() != 4 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "agent close frame must be 4 bytes",
                    ));
                }
                let channel_id =
                    u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
                Ok(Some(Frame::AgentClose { channel_id }))
            }
            TYPE_NEW_SESSION => Ok(Some(Frame::NewSession { name: decode_string(payload)? })),
            TYPE_ATTACH => Ok(Some(Frame::Attach { session: decode_string(payload)? })),
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
                let text: String =
                    vars.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("\n");
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
            Frame::NewSession { name } => encode_str(dst, TYPE_NEW_SESSION, &name),
            Frame::Attach { session } => encode_str(dst, TYPE_ATTACH, &session),
            Frame::ListSessions => encode_empty(dst, TYPE_LIST_SESSIONS),
            Frame::KillSession { session } => encode_str(dst, TYPE_KILL_SESSION, &session),
            Frame::KillServer => encode_empty(dst, TYPE_KILL_SERVER),
            Frame::SessionCreated { id } => encode_str(dst, TYPE_SESSION_CREATED, &id),
            Frame::SessionInfo { sessions } => {
                let text: String = sessions
                    .iter()
                    .map(|e| {
                        format!(
                            "{}\t{}\t{}\t{}\t{}\t{}\t{}",
                            e.id,
                            e.name,
                            e.pty_path,
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
