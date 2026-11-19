//! TCP + TLS connection helpers built on embassy-net and [`embedded_tls`].
//!
//! The TCP socket is split into independent reader and writer halves so a TLS
//! connection can itself be split with [`TlsConnection::split_with`] and read
//! and written concurrently (needed for the full-duplex Live API session).

use core::cell::UnsafeCell;

use alloc::vec::Vec;
use embassy_net::tcp::{TcpReader, TcpSocket, TcpWriter};
use embassy_net::{IpAddress, IpEndpoint, Stack};
use embassy_time::Duration;
use embedded_tls::{Aes128GcmSha256, NoVerify, TlsConfig, TlsConnection, TlsContext, TlsError};

use crate::println;

/// Largest TLS record (16 KiB plaintext) plus header, AEAD tag and padding.
const RECORD_READ_SIZE: usize = 16 * 1024 + 512;
const RECORD_WRITE_SIZE: usize = 16 * 1024;
const TCP_BUFFER_SIZE: usize = 16 * 1024;

#[derive(Debug)]
pub enum Error {
    Dns,
    Connect,
    Tls(TlsError),
    Io,
    Protocol(&'static str),
}

impl From<TlsError> for Error {
    fn from(value: TlsError) -> Self {
        Error::Tls(value)
    }
}

/// Heap allocated (PSRAM) buffers for one TCP + TLS connection.
pub struct Buffers {
    pub tcp_rx: Vec<u8>,
    pub tcp_tx: Vec<u8>,
    pub record_rx: Vec<u8>,
    pub record_tx: Vec<u8>,
}

impl Buffers {
    pub fn new() -> Self {
        Self {
            tcp_rx: alloc::vec![0; TCP_BUFFER_SIZE],
            tcp_tx: alloc::vec![0; TCP_BUFFER_SIZE],
            record_rx: alloc::vec![0; RECORD_READ_SIZE],
            record_tx: alloc::vec![0; RECORD_WRITE_SIZE],
        }
    }
}

impl Default for Buffers {
    fn default() -> Self {
        Self::new()
    }
}

pub async fn resolve(stack: Stack<'_>, host: &str) -> Result<IpAddress, Error> {
    let addresses = stack
        .dns_query(host, smoltcp::wire::DnsQueryType::A)
        .await
        .map_err(|_| Error::Dns)?;
    addresses.first().copied().ok_or(Error::Dns)
}

/// Resolves `host` and opens a TCP connection to it.
pub async fn connect_tcp<'a>(
    stack: Stack<'a>,
    host: &str,
    port: u16,
    rx: &'a mut [u8],
    tx: &'a mut [u8],
) -> Result<TcpSocket<'a>, Error> {
    let address = resolve(stack, host).await?;
    let mut socket = TcpSocket::new(stack, rx, tx);
    socket.set_timeout(Some(Duration::from_secs(30)));
    socket
        .connect(IpEndpoint::new(address, port))
        .await
        .map_err(|e| {
            println!("TCP connect to {host} failed: {e:?}");
            Error::Connect
        })?;
    Ok(socket)
}

/// Reader and writer half of a TCP socket that can be shared by [`SharedSocket`] handles.
pub struct SocketHalves<'a> {
    reader: UnsafeCell<TcpReader<'a>>,
    writer: UnsafeCell<TcpWriter<'a>>,
}

impl<'a> SocketHalves<'a> {
    pub fn new(socket: &'a mut TcpSocket<'_>) -> Self {
        let (reader, writer) = socket.split();
        Self {
            reader: UnsafeCell::new(reader),
            writer: UnsafeCell::new(writer),
        }
    }

    pub fn socket(&self) -> SharedSocket<'_, 'a> {
        SharedSocket { halves: self }
    }
}

/// Cloneable handle to [`SocketHalves`]: reads go to the reader half, writes to the writer half.
///
/// At most one clone may read and at most one clone may write at any time. This
/// holds for an unsplit [`TlsConnection`] and for its split `TlsReader` (only
/// reads) and `TlsWriter` (only writes).
#[derive(Clone, Copy)]
pub struct SharedSocket<'s, 'a> {
    halves: &'s SocketHalves<'a>,
}

impl embedded_io_async::ErrorType for SharedSocket<'_, '_> {
    type Error = embassy_net::tcp::Error;
}

impl embedded_io_async::Read for SharedSocket<'_, '_> {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        let reader = unsafe { &mut *self.halves.reader.get() };
        reader.read(buf).await
    }
}

impl embedded_io_async::Write for SharedSocket<'_, '_> {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        let writer = unsafe { &mut *self.halves.writer.get() };
        writer.write(buf).await
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        let writer = unsafe { &mut *self.halves.writer.get() };
        writer.flush().await
    }
}

pub type Tls<'b, 's, 'a> = TlsConnection<'b, SharedSocket<'s, 'a>, Aes128GcmSha256>;

/// Performs the TLS handshake on top of an established TCP connection.
///
/// Server certificates are not verified (see README).
pub async fn open_tls<'b, 's, 'a>(
    socket: SharedSocket<'s, 'a>,
    host: &str,
    record_rx: &'b mut [u8],
    record_tx: &'b mut [u8],
) -> Result<Tls<'b, 's, 'a>, Error>
where
    's: 'b,
    'a: 'b,
{
    let config = TlsConfig::<Aes128GcmSha256>::new().with_server_name(host);
    let mut tls = TlsConnection::new(socket, record_rx, record_tx);
    tls.open::<EspRng, NoVerify>(TlsContext::new(&config, &mut EspRng))
        .await?;
    Ok(tls)
}

/// Hardware RNG adapter for embedded-tls.
pub struct EspRng;

impl rand_core::RngCore for EspRng {
    fn next_u32(&mut self) -> u32 {
        esp_hal::rng::Rng::new().random()
    }

    fn next_u64(&mut self) -> u64 {
        ((esp_hal::rng::Rng::new().random() as u64) << 32)
            | esp_hal::rng::Rng::new().random() as u64
    }

    fn fill_bytes(&mut self, dst: &mut [u8]) {
        esp_hal::rng::Rng::new().read(dst);
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
        esp_hal::rng::Rng::new().read(dest);
        Ok(())
    }
}

impl rand_core::CryptoRng for EspRng {}
