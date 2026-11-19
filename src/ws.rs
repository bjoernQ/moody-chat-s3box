//! Minimal WebSocket (RFC 6455) client on top of any `embedded-io-async` stream.
//!
//! Supports the opening handshake, masked client frames, fragmented server
//! messages and ping/pong. Text and binary messages are treated the same way
//! since the Gemini Live API may deliver JSON in binary frames.

use alloc::string::String;
use alloc::vec::Vec;
use base64::Engine as _;
use embedded_io_async::{Read, Write};

use crate::println;
use crate::tls::Error;

const OP_CONTINUATION: u8 = 0x0;
const OP_TEXT: u8 = 0x1;
const OP_BINARY: u8 = 0x2;
const OP_CLOSE: u8 = 0x8;
const OP_PING: u8 = 0x9;
const OP_PONG: u8 = 0xA;

const MAX_HEADER_SIZE: usize = 8 * 1024;
const MAX_MESSAGE_SIZE: usize = 1024 * 1024;
const READ_CHUNK: usize = 4096;

/// Result of a successful opening handshake.
pub struct Handshake {
    /// Value of the HTTP `Date` response header, if present.
    pub date: Option<String>,
    /// Bytes received after the HTTP response header (start of the first frame).
    pub leftover: Vec<u8>,
}

/// Sends the HTTP upgrade request for `path` and waits for `101 Switching Protocols`.
pub async fn handshake<T: Read + Write>(
    io: &mut T,
    host: &str,
    path: &str,
) -> Result<Handshake, Error> {
    let mut key = [0u8; 16];
    esp_hal::rng::Rng::new().read(&mut key);
    let key = base64::engine::general_purpose::STANDARD.encode(key);

    let request = alloc::format!(
        "GET {path} HTTP/1.1\r\n\
         Host: {host}\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: {key}\r\n\
         Sec-WebSocket-Version: 13\r\n\r\n"
    );
    io.write_all(request.as_bytes())
        .await
        .map_err(|_| Error::Io)?;
    io.flush().await.map_err(|_| Error::Io)?;

    let mut response = Vec::new();
    let header_end = loop {
        if let Some(pos) = find(&response, b"\r\n\r\n") {
            break pos + 4;
        }
        if response.len() > MAX_HEADER_SIZE {
            return Err(Error::Protocol("handshake response too large"));
        }
        let len = response.len();
        response.resize(len + READ_CHUNK, 0);
        let n = io
            .read(&mut response[len..])
            .await
            .map_err(|_| Error::Io)?;
        response.truncate(len + n);
        if n == 0 {
            return Err(Error::Protocol("connection closed during handshake"));
        }
    };

    let header = core::str::from_utf8(&response[..header_end]).unwrap_or_default();
    let status_ok = header
        .lines()
        .next()
        .is_some_and(|line| line.contains(" 101"));
    if !status_ok {
        println!(
            "WebSocket handshake rejected: {}",
            core::str::from_utf8(&response).unwrap_or("<binary>")
        );
        return Err(Error::Protocol("handshake rejected"));
    }

    let date = header.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim()
            .eq_ignore_ascii_case("date")
            .then(|| String::from(value.trim()))
    });

    Ok(Handshake {
        date,
        leftover: response[header_end..].to_vec(),
    })
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Sending half of a WebSocket connection.
pub struct WsWriter<W> {
    io: W,
    frame: Vec<u8>,
}

impl<W: Write> WsWriter<W> {
    pub fn new(io: W) -> Self {
        Self {
            io,
            frame: Vec::new(),
        }
    }

    pub async fn send_text(&mut self, payload: &[u8]) -> Result<(), Error> {
        self.send_frame(OP_TEXT, payload).await
    }

    pub async fn send_pong(&mut self, payload: &[u8]) -> Result<(), Error> {
        self.send_frame(OP_PONG, payload).await
    }

    pub async fn send_close(&mut self) -> Result<(), Error> {
        self.send_frame(OP_CLOSE, &1000u16.to_be_bytes()).await
    }

    async fn send_frame(&mut self, opcode: u8, payload: &[u8]) -> Result<(), Error> {
        let mut mask = [0u8; 4];
        esp_hal::rng::Rng::new().read(&mut mask);

        self.frame.clear();
        self.frame.push(0x80 | opcode);
        let len = payload.len();
        if len < 126 {
            self.frame.push(0x80 | len as u8);
        } else if len <= u16::MAX as usize {
            self.frame.push(0x80 | 126);
            self.frame.extend_from_slice(&(len as u16).to_be_bytes());
        } else {
            self.frame.push(0x80 | 127);
            self.frame.extend_from_slice(&(len as u64).to_be_bytes());
        }
        self.frame.extend_from_slice(&mask);
        self.frame.extend(
            payload
                .iter()
                .enumerate()
                .map(|(i, byte)| byte ^ mask[i % 4]),
        );

        self.io
            .write_all(&self.frame)
            .await
            .map_err(|_| Error::Io)?;
        self.io.flush().await.map_err(|_| Error::Io)
    }
}

/// A complete message received from the server.
pub enum Incoming<'m> {
    /// Text or binary message payload.
    Data(&'m [u8]),
    Ping(Vec<u8>),
    Close,
}

/// Receiving half of a WebSocket connection.
pub struct WsReader<R> {
    io: R,
    pending: Vec<u8>,
    pos: usize,
    message: Vec<u8>,
}

impl<R: Read> WsReader<R> {
    pub fn new(io: R, leftover: Vec<u8>) -> Self {
        Self {
            io,
            pending: leftover,
            pos: 0,
            message: Vec::new(),
        }
    }

    pub async fn next(&mut self) -> Result<Incoming<'_>, Error> {
        loop {
            self.fill(2).await?;
            let b0 = self.pending[self.pos];
            let b1 = self.pending[self.pos + 1];
            let fin = b0 & 0x80 != 0;
            let opcode = b0 & 0x0F;
            let masked = b1 & 0x80 != 0;

            let mut header_len = 2;
            let len = match b1 & 0x7F {
                126 => {
                    header_len += 2;
                    self.fill(header_len).await?;
                    let p = self.pos;
                    u16::from_be_bytes([self.pending[p + 2], self.pending[p + 3]]) as usize
                }
                127 => {
                    header_len += 8;
                    self.fill(header_len).await?;
                    let p = self.pos;
                    let mut bytes = [0u8; 8];
                    bytes.copy_from_slice(&self.pending[p + 2..p + 10]);
                    u64::from_be_bytes(bytes) as usize
                }
                short => short as usize,
            };
            let mask_offset = header_len;
            if masked {
                header_len += 4;
            }
            if len > MAX_MESSAGE_SIZE || self.message.len() + len > MAX_MESSAGE_SIZE {
                return Err(Error::Protocol("message too large"));
            }

            self.fill(header_len + len).await?;
            let start = self.pos + header_len;
            if masked {
                let mut mask = [0u8; 4];
                mask.copy_from_slice(&self.pending[self.pos + mask_offset..][..4]);
                for (i, byte) in self.pending[start..start + len].iter_mut().enumerate() {
                    *byte ^= mask[i % 4];
                }
            }

            let frame_end = start + len;
            match opcode {
                OP_TEXT | OP_BINARY | OP_CONTINUATION => {
                    if opcode != OP_CONTINUATION {
                        self.message.clear();
                    }
                    self.message
                        .extend_from_slice(&self.pending[start..frame_end]);
                    self.consume(frame_end);
                    if fin {
                        return Ok(Incoming::Data(&self.message));
                    }
                }
                OP_PING => {
                    let payload = self.pending[start..frame_end].to_vec();
                    self.consume(frame_end);
                    return Ok(Incoming::Ping(payload));
                }
                OP_CLOSE => {
                    if len >= 2 {
                        let code = u16::from_be_bytes([self.pending[start], self.pending[start + 1]]);
                        let reason =
                            core::str::from_utf8(&self.pending[start + 2..frame_end]).unwrap_or("");
                        println!("WebSocket closed by server: {code} {reason}");
                    }
                    self.consume(frame_end);
                    return Ok(Incoming::Close);
                }
                _ => self.consume(frame_end),
            }
        }
    }

    /// Makes sure at least `n` unconsumed bytes are buffered.
    async fn fill(&mut self, n: usize) -> Result<(), Error> {
        while self.pending.len() - self.pos < n {
            let len = self.pending.len();
            self.pending.resize(len + READ_CHUNK, 0);
            let read = self.io.read(&mut self.pending[len..]).await;
            match read {
                Ok(0) => {
                    self.pending.truncate(len);
                    return Err(Error::Protocol("connection closed"));
                }
                Ok(count) => self.pending.truncate(len + count),
                Err(_) => {
                    self.pending.truncate(len);
                    return Err(Error::Io);
                }
            }
        }
        Ok(())
    }

    fn consume(&mut self, end: usize) {
        self.pos = end;
        if self.pos == self.pending.len() {
            self.pending.clear();
            self.pos = 0;
        } else if self.pos > 64 * 1024 {
            self.pending.drain(..self.pos);
            self.pos = 0;
        }
    }
}
