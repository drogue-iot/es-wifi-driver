#![cfg_attr(not(feature = "std"), no_std)]
#![feature(async_fn_in_trait)]
#![feature(impl_trait_projections)]
#![allow(incomplete_features)]
#![doc = include_str!("../README.md")]
#![warn(missing_docs)]
mod fmt;
mod parser;
mod socket_pool;

use socket_pool::SocketPool;

use embedded_hal::digital::{InputPin, OutputPin};

use {
    core::fmt::{Debug, Write as FmtWrite},
    embassy_sync::{
        blocking_mutex::raw::NoopRawMutex,
        channel::{Channel, DynamicSender},
    },
    embassy_time::{block_for, with_timeout, Duration, Instant, Timer},
    embedded_hal_async::{digital::Wait, spi::*},
    embedded_nal_async::*,
    futures_intrusive::sync::LocalMutex,
    heapless::String,
    parser::{CloseResponse, ConnectResponse, JoinResponse, ReadResponse, WriteResponse},
};

type DriverMutex = NoopRawMutex;

/// Socket error variants
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum SocketError {
    /// Error opening socket
    OpenError,
    /// Error connecting with socket
    ConnectError,
    /// Error reading from socket
    ReadError,
    /// Error writing to socket
    WriteError,
    /// Error closing socket
    CloseError,
    /// Attempting to use closed socket
    SocketClosed,
}

/// WiFi join errors
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum JoinError {
    /// Invalid SSID
    InvalidSsid,
    /// Invalid passkey
    InvalidPassword,
    /// Unknown error
    Unknown,
    /// Error associating to AP
    UnableToAssociate,
}

/// Error type for driver
#[derive(Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum Error<SPI, CS, RESET, READY> {
    /// Chip select error
    CS(CS),
    /// Reset pin error
    Reset(RESET),
    /// SPI error
    SPI(SPI),
    /// Ready pin error
    READY(READY),
    /// Socket error
    Socket(SocketError),
    /// Join error
    Join(JoinError),
}

const NAK: u8 = 0x15;

macro_rules! command {
    ($size:tt, $($arg:tt)*) => ({
        //let mut c = String::new();
        //c
        let mut c = String::<$size>::new();
        write!(c, $($arg)*).unwrap();
        c.push_str("\r").unwrap();
        c
    })
}

struct Cs<'a, CS: OutputPin + 'a> {
    cs: &'a mut CS,
}

impl<'a, CS: OutputPin + 'a> Cs<'a, CS> {
    fn new(cs: &'a mut CS) -> Result<Self, CS::Error> {
        cs.set_low()?;
        block_for(Duration::from_micros(1000));
        Ok(Self { cs })
    }
}

impl<'a, CS: OutputPin + 'a> Drop for Cs<'a, CS> {
    fn drop(&mut self) {
        let _ = self.cs.set_high();
        block_for(Duration::from_micros(15));
    }
}

/// Es-WiFi driver state
struct DriverState<SPI, CS, RESET, WAKEUP, READY>
where
    SPI: SpiBus<u8>,
    CS: OutputPin,
    RESET: OutputPin,
    WAKEUP: OutputPin,
    READY: InputPin + Wait,
{
    spi: SPI,
    cs: CS,
    reset: RESET,
    wakeup: WAKEUP,
    ready: READY,
    socket_pool: SocketPool,
}

impl<SPI, CS, RESET, WAKEUP, READY> DriverState<SPI, CS, RESET, WAKEUP, READY>
where
    SPI: SpiBus<u8>,
    CS: OutputPin,
    RESET: OutputPin,
    WAKEUP: OutputPin,
    READY: InputPin + Wait,
{
    /// Create a new instance of the es-wifi driver using the provided peripheral and pins.
    fn new(spi: SPI, cs: CS, reset: RESET, wakeup: WAKEUP, ready: READY) -> Self {
        Self {
            spi,
            cs,
            reset,
            wakeup,
            ready,
            socket_pool: SocketPool::new(),
        }
    }

    async fn wakeup(&mut self) {
        self.wakeup.set_low().ok().unwrap();
        Timer::after(Duration::from_millis(50)).await;
        self.wakeup.set_high().ok().unwrap();
        Timer::after(Duration::from_millis(50)).await;
    }

    async fn reset(&mut self) {
        self.reset.set_low().ok().unwrap();
        Timer::after(Duration::from_millis(50)).await;
        self.reset.set_high().ok().unwrap();
        Timer::after(Duration::from_millis(50)).await;
    }

    async fn wait_ready(
        &mut self,
    ) -> Result<(), Error<SPI::Error, CS::Error, RESET::Error, READY::Error>> {
        while self.ready.is_low().map_err(Error::READY)? {
            self.ready.wait_for_any_edge().await.map_err(Error::READY)?;
        }
        Ok(())
    }

    async fn start(
        &mut self,
    ) -> Result<(), Error<SPI::Error, CS::Error, RESET::Error, READY::Error>> {
        info!("Starting eS-WiFi adapter!");

        self.reset().await;
        self.wakeup().await;

        let mut response = [0; 4];
        let mut pos = 0;

        self.wait_ready().await?;
        {
            let _cs = Cs::new(&mut self.cs).map_err(Error::CS)?;
            loop {
                if self.ready.is_low().map_err(Error::READY)? {
                    break;
                }

                if pos >= response.len() {
                    break;
                }

                let mut chunk = [0x0A, 0x0A];
                Self::spi_transfer(&mut self.spi, &mut chunk, &[0x0A, 0x0A]).await?;

                // reverse order going from 16 -> 2*8 bits
                if chunk[1] != NAK {
                    response[pos] = chunk[1];
                    pos += 1;
                }
                if chunk[0] != NAK {
                    response[pos] = chunk[0];
                    pos += 1;
                }
            }
        }

        let needle = &[b'\r', b'\n', b'>', b' '];

        if !response[0..pos].starts_with(needle) {
            info!(
                "eS-WiFi adapter failed to initialize {:?}",
                &response[0..pos]
            );
        } else {
            // disable verbosity
            let mut resp = [0; 16];
            self.send_string(command!(8, "MT=1"), &mut resp).await?;
            //self.state = State::Ready;
            info!("eS-WiFi adapter is ready");
        }

        Ok(())
    }

    async fn join_wep(&mut self, ssid: &str, password: &str) -> Result<IpAddr, JoinError> {
        let mut response = [0; 1024];

        self.send_string(command!(36, "CB=2"), &mut response)
            .await
            .map_err(|_| JoinError::InvalidSsid)?;

        self.send_string(command!(36, "C1={}", ssid), &mut response)
            .await
            .map_err(|_| JoinError::InvalidSsid)?;

        self.send_string(command!(72, "C2={}", password), &mut response)
            .await
            .map_err(|_| JoinError::InvalidPassword)?;

        self.send_string(command!(8, "C3=4"), &mut response)
            .await
            .map_err(|_| JoinError::Unknown)?;

        let response = self
            .send_string(command!(4, "C0"), &mut response)
            .await
            .map_err(|_| JoinError::Unknown)?;

        //info!("[[{}]]", response);

        let parse_result = parser::join_response(&response);

        match parse_result {
            Ok((_, response)) => match response {
                JoinResponse::Ok(ip) => Ok(ip),
                JoinResponse::JoinError => Err(JoinError::UnableToAssociate),
            },
            Err(_) => {
                trace!("{:?}", &response);
                Err(JoinError::UnableToAssociate)
            }
        }
    }

    async fn send_string<'m, const N: usize>(
        &'m mut self,
        mut command: String<N>,
        response: &'m mut [u8],
    ) -> Result<&'m [u8], Error<SPI::Error, CS::Error, RESET::Error, READY::Error>> {
        if command.len() % 2 != 0 {
            command.push('\n').unwrap();
        }
        self.send(command.as_bytes(), response).await
    }

    async fn send<'m>(
        &'m mut self,
        command: &[u8],
        response: &'m mut [u8],
    ) -> Result<&'m [u8], Error<SPI::Error, CS::Error, RESET::Error, READY::Error>> {
        //trace!("send {:?}", core::str::from_utf8(&command[..]).unwrap());

        self.wait_ready().await?;
        {
            let _cs = Cs::new(&mut self.cs).map_err(Error::CS)?;
            for chunk in command.chunks(2) {
                let mut xfer: [u8; 2] = [0; 2];
                xfer[1] = chunk[0];
                if chunk.len() == 2 {
                    xfer[0] = chunk[1]
                } else {
                    xfer[0] = 0x0A
                }

                let a = xfer[0];
                let b = xfer[1];
                Self::spi_transfer(&mut self.spi, &mut xfer[..], &[a, b]).await?;
            }
            /*assert!(command.len() % 2 == 0);
            self.spi.transfer(&mut command[..]).map_err(SPI)?;*/
        }
        //info!("sent! awaiting response");

        self.receive(response).await
    }

    async fn spi_transfer(
        spi: &mut SPI,
        rx: &mut [u8],
        _tx: &[u8],
    ) -> Result<(), Error<SPI::Error, CS::Error, RESET::Error, READY::Error>> {
        spi.transfer_in_place(rx).await.map_err(Error::SPI)?;
        Ok(())
    }

    async fn receive<'m>(
        &'m mut self,
        response: &'m mut [u8],
    ) -> Result<&'m [u8], Error<SPI::Error, CS::Error, RESET::Error, READY::Error>> {
        let mut pos = 0;

        //trace!("Awaiting response ready");
        self.wait_ready().await?;
        //trace!("Response ready... reading");

        let _cs = Cs::new(&mut self.cs).map_err(Error::CS)?;

        while self.ready.is_high().map_err(Error::READY)? && response.len() - pos > 0 {
            //trace!("Receive pos({}), len({})", pos, response.len());

            let mut xfer: [u8; 2] = [0x0A, 0x0A];
            Self::spi_transfer(&mut self.spi, &mut xfer, &[0x0A, 0x0A]).await?;

            if xfer[0] == NAK {
                block_for(Duration::from_micros(1));
            }

            if !self.ready.is_high().map_err(Error::READY)? {
                if xfer[0] == NAK {
                    if xfer[1] != NAK {
                        response[pos] = xfer[1];
                        pos += 1;
                    }
                    break;
                }
            }
            response[pos] = xfer[1];
            pos += 1;

            response[pos] = xfer[0];
            pos += 1;
        }
        Ok(&response[0..pos])
    }

    async fn socket(&mut self) -> Result<u8, SocketError> {
        let h = self
            .socket_pool
            .open()
            .await
            .map_err(|_| SocketError::OpenError)?;
        trace!("Opened socket {}", h);
        Ok(h)
    }

    fn is_connected(&mut self, handle: u8) -> Result<bool, SocketError> {
        Ok(self.socket_pool.is_connected(handle))
    }

    async fn connect(&mut self, handle: u8, remote: SocketAddr) -> Result<(), SocketError> {
        let mut response = [0u8; 1024];
        let result = async {
            self.send_string(command!(8, "P0={}", handle), &mut response)
                .await
                .map_err(|_| {
                    trace!("[{}] CONNECT 1", handle);
                    SocketError::ConnectError
                })?;

            self.send_string(command!(8, "P1=0"), &mut response)
                .await
                .map_err(|_| {
                    trace!("[{}] CONNECT 2", handle);

                    SocketError::ConnectError
                })?;
            /*
            IpProtocol::Udp => {
                self.send_string(command!(8, "P1=1"), &mut response)
                    .await
                    .map_err(|_| SocketError::ConnectError)?;
            }
            */

            self.send_string(command!(32, "P3={}", remote.ip()), &mut response)
                .await
                .map_err(|_| {
                    trace!("[{}] CONNECT 3", handle);
                    SocketError::ConnectError
                })?;

            self.send_string(command!(32, "P4={}", remote.port()), &mut response)
                .await
                .map_err(|_| {
                    trace!("[{}] CONNECT 4", handle);
                    SocketError::ConnectError
                })?;

            let response = self
                .send_string(command!(8, "P6=1"), &mut response)
                .await
                .map_err(|_| {
                    trace!("[{}] CONNECT 5", handle);
                    SocketError::ConnectError
                })?;

            match parser::connect_response(&response) {
                Ok((_, ConnectResponse::Ok)) => {
                    self.socket_pool.set_connected(handle);
                    Ok(())
                }
                Ok((_, _)) => {
                    trace!("[{}] CONNECT 6", handle);
                    Err(SocketError::ConnectError)
                }
                Err(_) => {
                    trace!("[{}] CONNECT 7", handle);
                    Err(SocketError::ConnectError)
                }
            }
        }
        .await;
        result
    }

    async fn write(&mut self, handle: u8, buf: &[u8]) -> Result<usize, SocketError> {
        let mut response = [0u8; 32];
        let mut remaining = buf.len();
        trace!("Write request with {} bytes", remaining);
        self.send_string(command!(8, "P0={}", handle), &mut response)
            .await
            .map_err(|_| SocketError::WriteError)?;
        while remaining > 0 {
            // info!("Writing buf with len {}", len);

            let to_send = core::cmp::min(1200, remaining);
            trace!("Writing {} bytes to adapter", to_send);

            async {
                let mut prefix = command!(16, "S3={}", to_send).into_bytes();

                let (prefix, data) = if prefix.len() % 2 == 0 {
                    (&prefix[..], &buf[..to_send])
                } else {
                    prefix.push(buf[0]).unwrap();
                    (&prefix[..], &buf[1..to_send])
                };

                self.wait_ready()
                    .await
                    .map_err(|_| SocketError::WriteError)?;

                {
                    let _cs = Cs::new(&mut self.cs).map_err(|_| SocketError::WriteError)?;

                    trace!("Writing prefix of {} bytes", prefix.len());
                    for chunk in prefix.chunks(2) {
                        let mut xfer: [u8; 2] = [0; 2];
                        xfer[1] = chunk[0];
                        if chunk.len() == 2 {
                            xfer[0] = chunk[1]
                        } else {
                            xfer[0] = 0x0A
                        }

                        let a = xfer[0];
                        let b = xfer[1];

                        Self::spi_transfer(&mut self.spi, &mut xfer, &[a, b])
                            .await
                            .map_err(|_| SocketError::WriteError)?;
                    }

                    trace!("Writing data of {} bytes", data.len());
                    for chunk in data.chunks(2) {
                        let mut xfer: [u8; 2] = [0; 2];
                        xfer[1] = chunk[0];
                        if chunk.len() == 2 {
                            xfer[0] = chunk[1]
                        } else {
                            xfer[0] = 0x0A
                        }

                        let a = xfer[0];
                        let b = xfer[1];

                        Self::spi_transfer(&mut self.spi, &mut xfer, &[a, b])
                            .await
                            .map_err(|_| SocketError::WriteError)?;
                    }
                }

                let response = self
                    .receive(&mut response)
                    .await
                    .map_err(|_| SocketError::WriteError)?;

                if let Ok((_, WriteResponse::Ok(len))) = parser::write_response(response) {
                    remaining -= to_send;
                    Ok(len)
                } else {
                    trace!("Error reading response");
                    trace!("response:  {:?}", core::str::from_utf8(&response).unwrap());
                    Err(SocketError::WriteError)
                }
            }
            .await?;
        }
        Ok(buf.len())
    }

    async fn read(&mut self, handle: u8, buf: &mut [u8]) -> Result<usize, SocketError> {
        let mut pos = 0;
        //let buf_len = buf.len();
        loop {
            let result = async {
                let mut response = [0u8; 1470];

                self.send_string(command!(8, "P0={}", handle), &mut response)
                    .await
                    .map_err(|_| {
                        debug!("[{}] READ 1", handle);
                        SocketError::ReadError
                    })?;

                let maxlen = buf.len() - pos;
                let len = core::cmp::min(response.len() - 10, maxlen);

                self.send_string(command!(16, "R1={}", len), &mut response)
                    .await
                    .map_err(|_| {
                        debug!("[{}] READ 2", handle);
                        SocketError::ReadError
                    })?;

                /*
                self.send_string(&command!(8, "R2=10000"), &mut response)
                    .await
                    .map_err(|_| SocketError::ReadError)?;
                */

                self.send_string(command!(8, "R3=1"), &mut response)
                    .await
                    .map_err(|_| {
                        debug!("[{}] READ 3", handle);
                        SocketError::ReadError
                    })?;

                self.wait_ready().await.map_err(|_| {
                    debug!("[{}] READ 4", handle);
                    SocketError::ReadError
                })?;

                {
                    let _cs = Cs::new(&mut self.cs).map_err(|_| {
                        debug!("[{}] READ 5", handle);
                        SocketError::ReadError
                    })?;

                    let mut xfer = [b'0', b'R'];
                    Self::spi_transfer(&mut self.spi, &mut xfer, &[b'0', b'R'])
                        .await
                        .map_err(|_| {
                            debug!("[{}] READ 6", handle);
                            SocketError::ReadError
                        })?;

                    xfer = [b'\n', b'\r'];
                    Self::spi_transfer(&mut self.spi, &mut xfer, &[b'\n', b'\r'])
                        .await
                        .map_err(|_| {
                            debug!("[{}] READ 7", handle);
                            SocketError::ReadError
                        })?;
                }

                trace!(
                    "Receiving {} bytes, total buffer size is {}, pos is {}",
                    len,
                    buf.len(),
                    pos
                );
                let response = self.receive(&mut response).await.map_err(|_| {
                    debug!("[{}] READ 8", handle);
                    SocketError::ReadError
                })?;

                trace!("Response is {} bytes", response.len());
                //trace!("{:02x}", response);

                match parser::parse_response(&response) {
                    Ok((_, ReadResponse::Ok(data))) => {
                        if pos + data.len() > buf.len() {
                            trace!(
                                "Buf len is {}, pos is {}, Len is {}, data len is {}",
                                buf.len(),
                                pos,
                                len,
                                data.len()
                            );
                            if let Ok(s) = core::str::from_utf8(&data) {
                                trace!("response parsed:  {:?}", s);
                            }
                            trace!("response raw data: {:?}", response);
                            Err(SocketError::ReadError)
                        } else {
                            for (i, b) in data.iter().enumerate() {
                                buf[pos + i] = *b;
                            }
                            trace!("Read {} bytes", data.len());
                            Ok(data.len())
                        }
                    }
                    Ok((_, ReadResponse::Err)) => {
                        trace!("[{}] READ 9 ReadResponse::Err", handle);
                        //      warn!("response raw data: {:02x}", response);
                        Err(SocketError::ReadError)
                    }
                    _ => {
                        warn!("[{}] READ 9 parse error", handle);
                        if let Ok(s) = core::str::from_utf8(&response[..]) {
                            trace!("response parsed:  {:?}", s);
                        }
                        trace!("response raw data: {:?}", response);
                        Err(SocketError::ReadError)
                    }
                }
            }
            .await;

            match result {
                Ok(len) => {
                    pos += len;
                    if len == 0 || pos == buf.len() {
                        return Ok(pos);
                    }
                }
                Err(e) => {
                    if pos == 0 {
                        return Err(e);
                    } else {
                        return Ok(pos);
                    }
                }
            }
        }
    }

    async fn close(&mut self, handle: u8) -> Result<(), SocketError> {
        trace!("Closing connection for {}", handle);
        self.socket_pool.close(handle);
        let mut response = [0u8; 32];

        self.send_string(command!(8, "P0={}", handle), &mut response)
            .await
            .map_err(|_| {
                debug!("[{}] CLOSE 1", handle);
                SocketError::CloseError
            })?;

        let response = self
            .send_string(command!(8, "P6=0"), &mut response)
            .await
            .map_err(|_| {
                debug!("[{}] CLOSE 2", handle);
                SocketError::CloseError
            })?;

        match parser::close_response(&response) {
            Ok((_, CloseResponse::Ok)) => {
                debug!("[{}] Connection closed", handle);
                self.socket_pool.close(handle);
                Ok(())
            }
            Ok((_, _)) => {
                debug!("[{}] Error1 closing connection", handle);
                /*info!(
                    "[{}] close response:  {:?}",
                    handle,
                    core::str::from_utf8(&response).unwrap()
                );*/
                self.socket_pool.close(handle);
                Err(SocketError::CloseError)
            }
            Err(_) => {
                debug!("[{}] Error2 closing connection", handle);
                //info!("[{}] close response: {:x}", handle, response,);
                if let Ok(s) = core::str::from_utf8(&response) {
                    debug!("response parsed:  {:?}", s);
                }
                self.socket_pool.close(handle);
                Err(SocketError::CloseError)
            }
        }
    }
}

/// eS-WiFi driver.
pub struct EsWifi<SPI, CS, RESET, WAKEUP, READY>
where
    SPI: SpiBus<u8>,
    CS: OutputPin,
    RESET: OutputPin,
    WAKEUP: OutputPin,
    READY: InputPin + Wait,
{
    adapter: LocalMutex<DriverState<SPI, CS, RESET, WAKEUP, READY>>,
    control: Channel<DriverMutex, Control, 1>,
}

impl<SPI, CS, RESET, WAKEUP, READY> EsWifi<SPI, CS, RESET, WAKEUP, READY>
where
    SPI: SpiBus<u8>,
    CS: OutputPin,
    RESET: OutputPin,
    WAKEUP: OutputPin,
    READY: InputPin + Wait,
{
    /// Create a new instance of the driver.
    pub fn new(spi: SPI, cs: CS, reset: RESET, wakeup: WAKEUP, ready: READY) -> Self {
        let state = DriverState::new(spi, cs, reset, wakeup, ready);
        Self {
            adapter: LocalMutex::new(state, true),
            control: Channel::new(),
        }
    }

    async fn new_socket(&self) -> Result<u8, SocketError> {
        let mut adapter = self.adapter.lock().await;
        let handle = adapter.socket().await?;
        Ok(handle)
    }

    async fn reset(
        &self,
        ssid: &str,
        psk: &str,
    ) -> Result<(), Error<SPI::Error, CS::Error, RESET::Error, READY::Error>> {
        let mut adapter = self.adapter.lock().await;
        adapter.start().await?;
        debug!("Joining WiFi network...");
        adapter
            .join_wep(ssid, psk)
            .await
            .map_err(|e| Error::Join(e))?;
        debug!("WiFi network joined");
        Ok(())
    }

    /// Run driver stack
    pub async fn run(
        &self,
        ssid: &str,
        psk: &str,
    ) -> Result<(), Error<SPI::Error, CS::Error, RESET::Error, READY::Error>> {
        self.reset(ssid, psk).await?;
        loop {
            match self.control.recv().await {
                Control::Close(id) => {
                    let mut retries = 3;
                    while retries > 0 {
                        let mut adapter = self.adapter.lock().await;
                        match with_timeout(Duration::from_secs(10), adapter.close(id)).await {
                            Ok(r) => {
                                if let Err(e) = r {
                                    warn!("Error closing connection {}: {:?}", id, e);
                                    Timer::after(Duration::from_millis(50)).await;
                                    retries -= 1;
                                } else {
                                    break;
                                }
                            }
                            Err(_) => {
                                warn!("Timed out closing connection");
                                Timer::after(Duration::from_millis(50)).await;
                                retries -= 1;
                            }
                        }
                    }
                    // Resetting adapter to get it out of the bad state.
                    if retries == 0 {
                        self.reset(ssid, psk).await?;
                    }
                }
            }
        }
    }
}

/// Socket representing a single connection.
pub struct EsWifiSocket<'a, SPI, CS, RESET, WAKEUP, READY>
where
    SPI: SpiBus<u8> + 'a,
    CS: OutputPin + 'a,
    RESET: OutputPin + 'a,
    WAKEUP: OutputPin + 'a,
    READY: InputPin + Wait + 'a,
{
    handle: u8,
    adapter: &'a EsWifi<SPI, CS, RESET, WAKEUP, READY>,
    control: DynamicSender<'a, Control>,
    connect_timeout: Duration,
}

impl<SPI, CS, RESET, WAKEUP, READY> embedded_nal_async::TcpConnect
    for EsWifi<SPI, CS, RESET, WAKEUP, READY>
where
    SPI: SpiBus<u8>,
    CS: OutputPin,
    RESET: OutputPin,
    WAKEUP: OutputPin,
    READY: InputPin + Wait,
{
    type Error = SocketError;
    type Connection<'m> = EsWifiSocket<'m, SPI, CS, RESET, WAKEUP, READY> where Self: 'm;

    async fn connect<'m>(&'m self, remote: SocketAddr) -> Result<Self::Connection<'m>, Self::Error>
    where
        Self: 'm,
    {
        let handle = self.new_socket().await?;
        let mut socket = EsWifiSocket {
            handle,
            adapter: self,
            control: self.control.sender().into(),
            connect_timeout: Duration::from_secs(60),
        };
        socket.connect(remote).await?;
        Ok(socket)
    }
}

impl<'a, SPI, CS, RESET, WAKEUP, READY> EsWifiSocket<'a, SPI, CS, RESET, WAKEUP, READY>
where
    SPI: SpiBus<u8> + 'a,
    CS: OutputPin + 'a,
    RESET: OutputPin + 'a,
    WAKEUP: OutputPin + 'a,
    READY: InputPin + Wait + 'a,
{
    async fn connect(&mut self, remote: SocketAddr) -> Result<(), SocketError> {
        let timeout = Instant::now() + self.connect_timeout;
        while Instant::now() < timeout {
            let mut adapter = self.adapter.adapter.lock().await;

            if adapter.is_connected(self.handle)? {
                adapter.close(self.handle).await?;
            }

            match with_timeout(self.connect_timeout, adapter.connect(self.handle, remote)).await {
                Ok(Err(_e)) => {
                    Timer::after(Duration::from_millis(100)).await;
                }
                Ok(r) => return r,
                Err(_) => return Err(SocketError::ConnectError),
            }
        }
        Err(SocketError::ConnectError)
    }
}

impl<'a, SPI, CS, RESET, WAKEUP, READY> embedded_io::Io
    for EsWifiSocket<'a, SPI, CS, RESET, WAKEUP, READY>
where
    SPI: SpiBus<u8> + 'a,
    CS: OutputPin + 'a,
    RESET: OutputPin + 'a,
    WAKEUP: OutputPin + 'a,
    READY: InputPin + Wait + 'a,
{
    type Error = SocketError;
}

impl embedded_io::Error for SocketError {
    fn kind(&self) -> embedded_io::ErrorKind {
        embedded_io::ErrorKind::Other
    }
}

impl<'a, SPI, CS, RESET, WAKEUP, READY> embedded_io::asynch::Write
    for EsWifiSocket<'a, SPI, CS, RESET, WAKEUP, READY>
where
    SPI: SpiBus<u8> + 'a,
    CS: OutputPin + 'a,
    RESET: OutputPin + 'a,
    WAKEUP: OutputPin + 'a,
    READY: InputPin + Wait + 'a,
{
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        let mut adapter = self.adapter.adapter.lock().await;
        adapter.write(self.handle, buf).await
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

impl<'a, SPI, CS, RESET, WAKEUP, READY> embedded_io::asynch::Read
    for EsWifiSocket<'a, SPI, CS, RESET, WAKEUP, READY>
where
    SPI: SpiBus<u8> + 'a,
    CS: OutputPin + 'a,
    RESET: OutputPin + 'a,
    WAKEUP: OutputPin + 'a,
    READY: InputPin + Wait + 'a,
{
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        let mut adapter = self.adapter.adapter.lock().await;
        adapter.read(self.handle, buf).await
    }
}

impl<'a, SPI, CS, RESET, WAKEUP, READY> Drop for EsWifiSocket<'a, SPI, CS, RESET, WAKEUP, READY>
where
    SPI: SpiBus<u8> + 'a,
    CS: OutputPin + 'a,
    RESET: OutputPin + 'a,
    WAKEUP: OutputPin + 'a,
    READY: InputPin + Wait + 'a,
{
    fn drop(&mut self) {
        let _ = self.control.try_send(Control::Close(self.handle));
    }
}

enum Control {
    Close(u8),
}
