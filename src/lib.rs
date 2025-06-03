
use std::io::{self, Write, Read};
use std::net::TcpStream;
use std::thread;
use std::time::{Duration, Instant};
use serde::Serialize;
use serde::de::DeserializeOwned;
use thiserror::Error;


/// The number of bytes pulled from the TcpStream at a time. Smaller means more
/// system calls, larger means bulkier stack.
const POLL_SIZE: usize = 4096;

/// How long `receive_wait` waits between polls.
const WAIT_DELAY: Duration = Duration::from_millis(100);

/// How long `receive` waits by default before timing out in the case of
/// blocking.
const RECEIVE_DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// Wraps a TcpStream to provide an interface for sending arbitrary data over 
/// the network.
///  
/// # Security
/// The underlying socket communication is not encrypted. The user should avoid
/// using this library in unsecure environments.
pub struct TcpConn {
    stream: TcpStream,

    // VecDeque would be better because draining is faster, however any gains
    // are nullified due to the fact that there's currently no way in std to
    // construct a string from an iterator of bytes, thereby forcing a copy of
    // the data anyways by going through an intermediate container (issue occurs
    // in `receive`).
    buffer: Vec<u8>,

    nonblocking: bool,
}

/// The error type for TcpConn
#[derive(Error, Debug)]
pub enum TcpConnError {
    /// An error occurred with the socket
    #[error("An error occurred with the socket")]
    Socket(#[from] io::Error),
    
    /// The tranfer could not be completed within the specified window
    #[error("The tranfer could not be completed within the specified window")]
    Timeout,

    /// Unable to reconstruct type due to insufficient data
    #[error("Unable to reconstruct type due to insufficient data")]
    Incomplete,

    /// The payload transfer completed, but it could not be reassembled into the
    /// desired type
    #[error("The payload transfer completed, but it could not be reassembled \
into the desired type")]
    Corrupt,

    /// The type could not be serialized
    #[error("The type could not be serialized")]
    Serialize
}

impl TcpConn {
    /// Construct a `TcpConn` by wrapping a `TcpStream`. The `TcpStream` should
    /// be configured beforehand, with the exception of blocking. Blocking is
    /// enforced by default regardless of how the `TcpStream` was set before.
    /// This can be changed with `set_nonblocking`.
    /// 
    /// # Errors
    /// This function may return `TcpConnError::Socket`.
    pub fn new(stream: TcpStream) -> Result<Self, TcpConnError> {
        stream.set_nonblocking(false)?;
        Ok(Self {
            stream,
            buffer: Vec::new(),
            nonblocking: false
        })
    }

    /// Set the connection's blocking state. This affects both the underlying
    /// `TcpStream` and the way `receive` behaves. Non-blocking will allow
    /// `receive` to return early if a message has only partially arrived.
    /// 
    /// # Errors
    /// This function may return `TcpConnError::Socket`.
    pub fn set_nonblocking(&mut self, nonblocking: bool)
     -> Result<(), TcpConnError> {
        self.nonblocking = nonblocking;
        Ok(self.stream.set_nonblocking(nonblocking)?)
    }

    /// Empty the internal buffer of the connection. This may be necessary when
    /// recovering from an error returned by `receive`. For example, if
    /// `receive` returns the error `TcpConnError::Corrupt`, that probably means
    /// there is something wrong with the type sent across the network, but the
    /// buffer is still filled. The server may wish to discard the buffer so it
    /// can receive other messages.
    pub fn clear_buffer(&mut self) {
        self.buffer.clear()
    }

    /// Send an arbitrary message type over the socket connection.
    /// 
    /// # Errors
    /// This function may return `TcpConnError::Socket` or
    /// `TcpConnError::Serialize`.
    pub fn send<T>(&mut self, data: &T) -> Result<(), TcpConnError>
    where T: Serialize {
        let bytes = serde_cbor::to_vec(data)
                               .map_err(|_| TcpConnError::Serialize)?;
        let mut packet = bytes.len().to_le_bytes().to_vec();

        packet.extend(bytes);

        self.stream.write_all(&packet)?;
        self.stream.flush()?;

        Ok(())
    }

    /// Attempt to deserialize a type from the TCP stream. If the
    /// connection is blocking, it will wait up to `RECEIVE_DEFAULT_TIMEOUT`
    /// seconds.
    /// 
    /// # Errors
    /// If the connection is blocking, this function may return
    /// `TcpConnError::Incomplete`, `TcpConnError::Timeout`,
    /// `TcpConnError::Socket`, or `TcpConnError::Corrupt`.
    /// 
    /// If it is not blocking, it may return `TcpConnError::Timeout`,
    /// `TcpConnError::Socket`, or `TcpConnError::Corrupt`.
    pub fn receive<T>(&mut self) -> Result<T, TcpConnError>
    where T: DeserializeOwned {
        if self.nonblocking {
            self.receive_partial()
        } else {
            self.receive_full(RECEIVE_DEFAULT_TIMEOUT)
        }
    }

    /// Attempt to deserialize a type from the TCP stream with a custom timeout.
    /// This function blocks regardless of the blocking state.
    /// 
    /// # Errors
    /// This function may return `TcpConnError::Timeout`,
    /// `TcpConnError::Socket`, or `TcpConnError::Corrupt`.
    pub fn receive_timeout<T>(&mut self, timeout: Duration)
     -> Result<T, TcpConnError>
    where T: DeserializeOwned {
        let old = self.nonblocking;
        self.set_nonblocking(false)?;
        
        let r = self.receive_full(timeout);

        self.set_nonblocking(old)?;
        r
    }

    /// Receive the next incoming message, returning early with the error
    /// `TcpConnError::Incomplete` if the entire message has not yet arrived.
    /// 
    /// # Errors
    /// This function may return `TcpConnError::Socket`,
    /// `TcpConnError::Incomplete`, or `TcpConnError::Corrupt`.
    fn receive_partial<T>(&mut self) -> Result<T, TcpConnError>
    where T: DeserializeOwned {

        // try receiving some data by polling the TcpStream until it is empty
        let mut readbuf = [0u8; POLL_SIZE];
        loop {
            // grab POLL_SIZE bytes from the TcpStream and add them to
            // self.buffer
            let bytes_read = self.stream.read(&mut readbuf)?;
            self.buffer.extend(&readbuf[..bytes_read]);

            // check if there are no more bytes to read (even if we don't have
            // enough bytes to deserialize into `T`)
            if bytes_read < POLL_SIZE {
                break;
            }
        }

        // attempt to read the 8 bytes representing the payload size
        let size_bytes: [u8; 8] = self.buffer.get(..8)
            .ok_or(TcpConnError::Incomplete)?
            .try_into()
            .map_err(|_| TcpConnError::Incomplete)?;
        
        let payload_size = usize::from_le_bytes(size_bytes);

        // make sure theres enough bytes to reconstruct the original data type
        if self.buffer.len() < payload_size + 8 {
            return Err(TcpConnError::Incomplete);
        }
        
        // deserialize the str into `T`
        let data = serde_cbor::from_slice(&self.buffer[8..payload_size + 8])
            .map_err(|_| TcpConnError::Corrupt)?;

        // remove size+8 bytes from the buffer. this is last because we don't
        // want to drain if the previous operations fail
        self.buffer.drain(..payload_size + 8);
        
        Ok(data)
    }

    /// Same as `receive_partial` except it spins with some delay until it
    /// receives the entire message.
    /// 
    /// # Errors
    /// This function may return `TcpConnError::Timeout`,
    /// `TcpConnError::Socket`, or `TcpConnError::Corrupt`.
    fn receive_full<T>(&mut self, timeout: Duration) -> Result<T, TcpConnError>
    where T: DeserializeOwned {
        let timeout_end = Instant::now() + timeout;
        loop {
            match self.receive_partial() {
                Ok(msg) => return Ok(msg),
                Err(TcpConnError::Incomplete) => {}
                Err(e) => return Err(e),
            }
            if Instant::now() >= timeout_end {
                return Err(TcpConnError::Timeout);
            }
            thread::sleep(WAIT_DELAY);
        }
    }
}
