use crate::error::{Error, Result};
use bytes::{BufMut, Bytes, BytesMut};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};

pub const PROTOCOL_VERSION: u8 = 1;
pub const MAX_FRAME_SIZE: usize = 1_048_576; // 1MB
pub const MAX_PAYLOAD_SIZE: usize = 32_768; // 32KB
pub const DEFAULT_WINDOW_SIZE: u64 = 65_536; // 64KB

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameType {
    Hello = 0x01,
    HelloAck = 0x02,
    Run = 0x10,
    RunAck = 0x11,
    Output = 0x20,
    Status = 0x30,
    Exit = 0x40,
    Cancel = 0x50,
    CancelAck = 0x51,
    WindowUpdate = 0x60,
    Error = 0xF0,
    Ping = 0xFE,
    Pong = 0xFF,
}

impl TryFrom<u8> for FrameType {
    type Error = Error;

    fn try_from(value: u8) -> Result<Self> {
        match value {
            0x01 => Ok(FrameType::Hello),
            0x02 => Ok(FrameType::HelloAck),
            0x10 => Ok(FrameType::Run),
            0x11 => Ok(FrameType::RunAck),
            0x20 => Ok(FrameType::Output),
            0x30 => Ok(FrameType::Status),
            0x40 => Ok(FrameType::Exit),
            0x50 => Ok(FrameType::Cancel),
            0x51 => Ok(FrameType::CancelAck),
            0x60 => Ok(FrameType::WindowUpdate),
            0xF0 => Ok(FrameType::Error),
            0xFE => Ok(FrameType::Ping),
            0xFF => Ok(FrameType::Pong),
            _ => Err(Error::Protocol(format!(
                "Unknown frame type: 0x{:02x}",
                value
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum StreamId {
    Control = 0,
    Stdout = 1,
    Stderr = 2,
    Stdin = 3,
}

impl TryFrom<u8> for StreamId {
    type Error = Error;

    fn try_from(value: u8) -> Result<Self> {
        match value {
            0 => Ok(StreamId::Control),
            1 => Ok(StreamId::Stdout),
            2 => Ok(StreamId::Stderr),
            3 => Ok(StreamId::Stdin),
            _ => Err(Error::Protocol(format!("Invalid stream ID: {}", value))),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Frame {
    pub frame_type: FrameType,
    pub stream_id: StreamId,
    pub flags: u16,
    pub job_id: Option<u32>,
    pub sequence_number: Option<u32>,
    pub payload: Bytes,
}

impl Frame {
    pub fn new(frame_type: FrameType, stream_id: StreamId) -> Self {
        Self {
            frame_type,
            stream_id,
            flags: 0,
            job_id: None,
            sequence_number: None,
            payload: Bytes::new(),
        }
    }

    pub fn with_job_id(mut self, job_id: u32) -> Self {
        self.job_id = Some(job_id);
        self
    }

    pub fn with_sequence(mut self, seq: u32) -> Self {
        self.sequence_number = Some(seq);
        self
    }

    pub fn with_payload(mut self, payload: Bytes) -> Self {
        self.payload = payload;
        self
    }

    pub fn with_flag(mut self, flag: u16) -> Self {
        self.flags |= flag;
        self
    }

    pub fn has_flag(&self, flag: u16) -> bool {
        self.flags & flag != 0
    }

    pub fn encode(&self) -> Result<Bytes> {
        let payload_len = self.payload.len();
        if payload_len > MAX_PAYLOAD_SIZE {
            return Err(Error::Protocol(format!(
                "Payload too large: {} bytes (max: {})",
                payload_len, MAX_PAYLOAD_SIZE
            )));
        }

        // Frame format:
        // [4 bytes: length] [1 byte: type] [1 byte: stream_id] [2 bytes: flags]
        // [4 bytes: job_id] [4 bytes: sequence] [variable: payload]
        let frame_len = 1 + 1 + 2 + 4 + 4 + payload_len;
        let total_len = 4 + frame_len; // Include length field

        if total_len > MAX_FRAME_SIZE {
            return Err(Error::Protocol(format!(
                "Frame too large: {} bytes (max: {})",
                total_len, MAX_FRAME_SIZE
            )));
        }

        let mut buf = BytesMut::with_capacity(total_len);
        buf.put_u32_le(frame_len as u32);
        buf.put_u8(self.frame_type as u8);
        buf.put_u8(self.stream_id as u8);
        buf.put_u16_le(self.flags);
        buf.put_u32_le(self.job_id.unwrap_or(0));
        buf.put_u32_le(self.sequence_number.unwrap_or(0));
        buf.put_slice(&self.payload);

        Ok(buf.freeze())
    }

    pub fn decode<R: Read>(reader: &mut R) -> Result<Self> {
        // Read length field (4 bytes)
        let mut len_buf = [0u8; 4];
        read_exact(reader, &mut len_buf)?;
        let frame_len = u32::from_le_bytes(len_buf) as usize;

        if frame_len > MAX_FRAME_SIZE - 4 {
            return Err(Error::Protocol(format!(
                "Frame length too large: {} bytes",
                frame_len
            )));
        }

        // Read frame header
        let mut header_buf = [0u8; 12];
        read_exact(reader, &mut header_buf)?;

        let frame_type = FrameType::try_from(header_buf[0])?;
        let stream_id = StreamId::try_from(header_buf[1])?;
        let flags = u16::from_le_bytes([header_buf[2], header_buf[3]]);
        let job_id_raw =
            u32::from_le_bytes([header_buf[4], header_buf[5], header_buf[6], header_buf[7]]);
        let sequence_raw =
            u32::from_le_bytes([header_buf[8], header_buf[9], header_buf[10], header_buf[11]]);

        let job_id = if job_id_raw != 0 {
            Some(job_id_raw)
        } else {
            None
        };
        let sequence_number = if sequence_raw != 0 {
            Some(sequence_raw)
        } else {
            None
        };

        // Read payload
        let payload_len = frame_len - 12;
        let mut payload = vec![0u8; payload_len];
        if payload_len > 0 {
            read_exact(reader, &mut payload)?;
        }

        Ok(Self {
            frame_type,
            stream_id,
            flags,
            job_id,
            sequence_number,
            payload: Bytes::from(payload),
        })
    }
}

fn read_exact<R: Read>(reader: &mut R, buf: &mut [u8]) -> Result<()> {
    let mut total_read = 0;
    while total_read < buf.len() {
        match reader.read(&mut buf[total_read..]) {
            Ok(0) => {
                return Err(Error::Protocol(
                    "Unexpected EOF while reading frame".to_string(),
                ));
            }
            Ok(n) => total_read += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(Error::Io(e)),
        }
    }
    Ok(())
}

// Frame payload types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloPayload {
    pub version: u8,
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunPayload {
    pub command_line: String,
    pub working_directory: Option<String>,
    pub environment: Option<std::collections::HashMap<String, String>>,
    pub timeout_sec: Option<u64>,
    pub run_as: Option<String>, // "system", "elevated", "user"
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunAckPayload {
    pub job_id: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusPayload {
    pub job_id: u32,
    pub status: String, // "Starting", "Running", "Exiting", "Completed", "Failed", "Canceled", "TimedOut"
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExitPayload {
    pub job_id: u32,
    pub exit_code: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorPayload {
    pub job_id: Option<u32>,
    pub error_code: u32,
    pub error_message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowUpdatePayload {
    pub stream_id: u8,
    pub bytes_consumed: u64,
}

// Frame flags
pub const FLAG_EOS: u16 = 0x01; // End of stream
pub const FLAG_URGENT: u16 = 0x02; // Urgent frame
