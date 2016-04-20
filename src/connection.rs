//! Interactions with the I/O layer and the HTTP/2 state machine
use std::io::{self, Cursor};
use std::fmt;

use mio::{self, EventSet, EventLoop, TryRead, TryWrite};

use protocol::{self, Protocol, Http2};
use request;
use worker::{self, Worker};

pub trait NetworkStreamError : Send + Sync + ::std::error::Error + 'static { }

impl NetworkStreamError for io::Error {}

pub enum StreamStatus {
    Bytes(usize),
    WantRead,
    WantWrite,
    WouldBlock,
    End,
}

type StreamResult<T, E> = ::std::result::Result<T, E>;

pub trait NetworkStream : Send {
    type Error: NetworkStreamError;

    fn async_read(&mut self, buf: &mut [u8]) -> StreamResult<StreamStatus, Box<Self::Error>>;
    fn async_write(&mut self, buf: &[u8]) -> StreamResult<StreamStatus, Box<Self::Error>>;
    fn get_ref(&self) -> &mio::Evented;
}

struct PlaintextStream {
    inner: mio::tcp::TcpStream,
}

impl PlaintextStream {
    pub fn new(stream: mio::tcp::TcpStream) -> PlaintextStream {
        PlaintextStream {
            inner: stream
        }
    }
}

impl NetworkStream for PlaintextStream {
    type Error = io::Error;

    fn async_read(&mut self, buf: &mut[u8]) -> StreamResult<StreamStatus, Box<Self::Error>> {
        match self.inner.try_read(buf) {
            Ok(Some(0)) => Ok(StreamStatus::End),
            Ok(Some(count)) => Ok(StreamStatus::Bytes(count)),
            Ok(None) => Ok(StreamStatus::WouldBlock),
            Err(err) => Err(Box::new(err)),
        }
    }

    fn async_write(&mut self, buf: &[u8]) -> StreamResult<StreamStatus, Box<Self::Error>> {
        match self.inner.try_write(buf) {
            Ok(Some(count)) => Ok(StreamStatus::Bytes(count)),
            Ok(None) => Ok(StreamStatus::WouldBlock),
            Err(err) => Err(Box::new(err)),
        }
    }

    fn get_ref(&self) -> &mio::Evented {
        &self.inner as &mio::Evented
    }
}

/// Type that receives events for a connection
pub trait Handler: Send + fmt::Debug + 'static {
    fn on_connection(&self, Handle);
    fn on_error(&self, Error);
    fn on_pong(&self);
}

/// Errors occurring in or when interacting with a connection
///
/// TODO separate internal errors
#[derive(Debug)]
pub enum Error {
    /// IO layer encountered an error.
    Io(io::Error),

    /// Error from the HTTP/2 state machine; the connection cannot be used for sending further
    /// requests.
    Http(::solicit::http::HttpError),

    /// Timeout while connecting
    Timeout,

    /// The worker is closing and a connection can not be created.
    WorkerClosing,

    /// The worker has reached its capacity for connections
    WorkerFull,

    /// Network error
    Stream(Box<NetworkStreamError>),
}

impl ::std::error::Error for Error {
    fn cause(&self) -> Option<&::std::error::Error> {
        match *self {
            Error::Io(ref err) => Some(err),
            Error::Http(ref err) => Some(err),
            Error::Stream(ref err) => err.cause(),
            _ => None,
        }
    }

    fn description(&self) -> &str {
        match *self {
            Error::Io(ref err) => err.description(),
            Error::Http(ref err) => err.description(),
            Error::Timeout => "timeout when attempting to connect",
            Error::WorkerClosing => "worker isn't accepting new connections",
            Error::WorkerFull => "worker cannot manage more connections",
            Error::Stream(ref err) => err.description(),
        }
    }
}

impl ::std::fmt::Display for Error {
    fn fmt(&self, f: &mut ::std::fmt::Formatter) -> ::std::fmt::Result {
        match *self {
            Error::Io(ref err) => write!(f, "I/O error on connection: {}", err),
            Error::Http(ref err) => write!(f, "HTTP/2 error on connection: {}", err),
            Error::Timeout => write!(f, "Timeout during connect"),
            Error::WorkerClosing => write!(f, "Worker not accepting new connections"),
            Error::WorkerFull => write!(f, "Worker at connection capacity"),
            Error::Stream(ref err) => write!(f, "{}", err),
        }
    }
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Error {
        Error::Io(err)
    }
}

impl From<::solicit::http::HttpError> for Error {
    fn from(val: ::solicit::http::HttpError) -> Error {
        Error::Http(val)
    }
}

impl<E: NetworkStreamError + 'static> From<Box<E>> for Error {
    fn from(val: Box<E>) -> Error {
        Error::Stream(val)
    }
}

/// Connection result type
pub type Result<T> = ::std::result::Result<T, Error>;

/// An HTTP/2 connection
///
/// Contains some sort of transport layer and the HTTP/2 state machine. I/O is buffered within this
/// type. Incoming bytes are stored in a `read_buf` when there are not enough available to create an
/// entire HTTP/2 frame. The `backlog` is a series of I/O objects representing pending outgoing
/// frames.
///
/// The worker manages a set of connections using an event loop.
pub struct Connection {
    token: mio::Token,
    stream: Box<NetworkStream<Error=NetworkStreamError>>,
    backlog: Vec<Cursor<Vec<u8>>>,
    is_writable: bool,
    protocol: Http2,
    read_buf: Vec<u8>,
    handler: Box<Handler>,
}

/// Status of last write
enum WriteStatus {
    /// Wrote all the bytes
    Full,

    /// Wrote some of the bytes
    Partial(usize),

    /// Can't write, write would block
    WouldBlock,
}

/// A projection of the Connection type for use within the worker thread.
pub struct Ref<'a> {
    backlog: &'a mut Vec<Cursor<Vec<u8>>>,
    handler: &'a Handler,
    event_loop: &'a mut EventLoop<Worker>,
    token: mio::Token,
}

/// Handle for a connection usable from other threads
///
/// To get a ConnectionHandle, a user must create a Hydra instance and establish a connection. The
/// connection handle is provided in the connection::Handler::on_connection method.
#[derive(Debug, Clone)]
pub struct Handle {
    // How many streams are currently active.
    //
    // This value is exposed to the client so they can hold on to the handle as long as there are
    // still requests active. Hmm. Maybe the event loop should just be smart enough to not close
    // while there's still activity? If these should really be exposed, a simple strategy would be
    // adding functions to the Handler type.
    // active_streams: Arc<AtomicUsize>,

    // Number of requests queued in the connection
    //
    // Should only be nonzero when active_streams matches max_concurrent_streams.
    // queued_requests: Arc<AtomicUsize>,

    /// Sender to event loop where connection is running.
    tx: mio::Sender<worker::Msg>,

    /// Token for the connection on its corresponding thread
    token: mio::Token,
}

/// Errors occurring when sending a message to the connection.
pub type SendError<T> = mio::NotifyError<T>;
pub type SendResult<T> = ::std::result::Result<(), SendError<T>>;

impl Handle {
    /// Send a message to the worker's event loop
    fn send(&self, msg: protocol::Msg) -> SendResult<worker::Msg> {
        self.tx.send(worker::Msg::Protocol(self.token, msg))
    }

    /// Notify the connection to create a new request stream from `req` and `handler`
    pub fn request<H>(&self, req: request::Request, handler: H) -> SendResult<worker::Msg>
        where H: request::Handler
    {
        self.send(protocol::Msg::CreateStream(req, Box::new(handler)))
    }
}

pub trait DispatchConnectionEvent {
    fn connection_ready(&self);
    fn on_pong(&self);
}

/// ------------------------------------------------------------------------------------------------
/// Ref impls
/// ------------------------------------------------------------------------------------------------

impl<'a> Ref<'a> {
    pub fn queue_frame(&mut self, frame: Vec<u8>) {
        trace!("Ref::queue_frame");
        self.backlog.push(Cursor::new(frame));
    }

    pub fn write_frame(&mut self, frame: Vec<u8>) {
        self.queue_frame(frame);
        // If idle, wake it up by posting a message;
        // If blocked, do not post anything
        // If in the middle of a write (i.e. this is the result of the Connection asking the
        // protocol to provide some data), do not post anything
    }
}

impl<'a> DispatchConnectionEvent for Ref<'a> {
    #[inline]
    fn connection_ready(&self) {
        let handle = Handle {
           tx: self.event_loop.channel(),
           token: self.token,
        };

        self.handler.on_connection(handle);
    }

    #[inline]
    fn on_pong(&self) {
        self.handler.on_pong();
    }
}

/// ------------------------------------------------------------------------------------------------
/// Connection impls
/// ------------------------------------------------------------------------------------------------

impl Connection {
    pub fn on_connect_timeout(&self) {
        self.on_error(Error::Timeout);
    }

    pub fn on_error(&self, err: Error) {
        self.handler.on_error(err);
    }

    #[inline]
    pub fn stream(&self) -> &mio::Evented {
        self.stream.get_ref()
    }

    pub fn new(token: mio::Token,
               stream: Box<NetworkStream<Error=NetworkStreamError>>,
               handler: Box<Handler>) -> Connection
    {
        trace!("Connection::new");
        Connection {
            token: token,
            stream: stream,
            backlog: Vec::new(),
            is_writable: false,
            protocol: Http2::new(),
            read_buf: Vec::with_capacity(4096),
            handler: handler,
        }
    }

    pub fn initialize(&mut self, event_loop: &mut EventLoop<Worker>) {
        trace!("Connection::initialize");
        let conn_ref = Ref {
            event_loop: event_loop,
            backlog: &mut self.backlog,
            token: self.token,
            handler: &*self.handler,
        };

        self.protocol.initialize(conn_ref);
    }

    pub fn ready(&mut self,
                 event_loop: &mut EventLoop<Worker>,
                 events: EventSet) -> Result<()>
    {
        trace!("Connection ready: token={:?}; events={:?}", self.token, events);
        if events.is_readable() {
            try!(self.read(event_loop));
        }
        if events.is_writable() {
            self.set_writable(true);
            // Whenever the connection becomes writable, we try a write.
            try!(self.try_write(event_loop));
        }

        // TODO handle error event

        try!(self.reregister(event_loop));

        Ok(())
    }

    fn reregister(&mut self, event_loop: &mut EventLoop<Worker>) -> Result<()> {
        let mut flags = mio::EventSet::readable();
        if !self.backlog.is_empty() || self.protocol.wants_write() {
            flags = flags | mio::EventSet::writable();
        }

        let pollopt = mio::PollOpt::edge() | mio::PollOpt::oneshot();
        trace!("reregistering {:?} for {:?}", self.token, flags);
        Ok(try!(event_loop.reregister(self.stream.get_ref(), self.token, flags, pollopt)))
    }

    pub fn deregister(&mut self, event_loop: &mut EventLoop<Worker>) -> Result<()> {
        Ok(try!(event_loop.deregister(self.stream.get_ref())))
    }

    pub fn notify(&mut self,
                  event_loop: &mut EventLoop<Worker>,
                  msg: protocol::Msg) -> Result<()>
    {
        // The scope is necessary here since otherwise the borrow checker thinks
        // the event_loop is borrowed too long and we can't call self.reregister.
        {
            let conn_ref = Ref {
                event_loop: event_loop,
                backlog: &mut self.backlog,
                token: self.token,
                handler: &*self.handler,
            };

            // Notify the protocol
            try!(self.protocol.notify(msg, conn_ref));
        }

        // Reregister this connection on the event loop
        try!(self.reregister(event_loop));

        Ok(())
    }

    fn read(&mut self, event_loop: &mut EventLoop<Worker>) -> Result<()> {
        // TODO Handle the case where the buffer isn't large enough to completely
        // exhaust the socket (since we're edge-triggered, we'd never get another
        // chance to read!)
        trace!("Handling read");
        match try!(self.stream.async_read(&mut self.read_buf)) {
            StreamStatus::End => {
                debug!("EOF");
                // TODO EOF; should close connection and any remaining streams
                unimplemented!();
            },
            StreamStatus::Bytes(n) => {
                debug!("read {} bytes", n);
                let consumed = {
                    let conn_ref = Ref {
                        event_loop: event_loop,
                        token: self.token,
                        backlog: &mut self.backlog,
                        handler: &*self.handler,
                    };
                    let buf = &self.read_buf;

                    try!(self.protocol.on_data(buf, conn_ref))
                };

                // TODO Would a circular buffer be better than draining here...?
                // Though it might not be ... there shouldn't be that many elems
                // to copy to the front usually... this would give an advantage
                // to later reads as there will never be a situation where a mini
                // read needs to be done because we're about to wrap around in the
                // buffer ... on the other hand, a mini read every-so-often might
                // even be okay? If it eliminates copies...
                trace!("Draining... {}/{}", consumed, self.read_buf.len());
                self.read_buf.drain(..consumed);

                trace!("Done.");
            },
            _ => {
                trace!("read WOULDBLOCK");
            },
        }

        // Reading data may allow for new streams to start. Give protocol a chance to write.
        {
            let conn_ref = Ref {
                event_loop: event_loop,
                token: self.token,
                backlog: &mut self.backlog,
                handler: &*self.handler,
            };
            try!(self.protocol.ready_write(conn_ref));
        }

        Ok(())
    }

    fn try_write(&mut self, event_loop: &mut EventLoop<Worker>) -> Result<()> {
        trace!("-> Attempting to write!");
        if !self.is_writable {
            trace!("Currently not writable!");
            return Ok(())
        }
        // Anything that's already been queued is considered the first priority to
        // push out to the peer. We write as much of this as possible.
        try!(self.write_backlog());
        if self.is_writable {
            trace!("Backlog flushed; notifying protocol");
            // If we're still writable, tell the protocol so that it can react to that and
            // potentially provide more.
            {
                let conn_ref = Ref {
                    event_loop: event_loop,
                    token: self.token,
                    backlog: &mut self.backlog,
                    handler: &*self.handler,
                };
                try!(self.protocol.ready_write(conn_ref));
            }

            // Flush whatever the protocol might have added...
            // TODO Should we allow the protocol to queue more than once (this could hold
            // up the event loop if the protocol keeps adding stuff...)
            try!(self.write_backlog());
        }

        Ok(())
    }

    fn write_backlog(&mut self) -> Result<()> {
        loop {
            if self.backlog.is_empty() {
                trace!("Backlog already empty.");
                return Ok(());
            }
            trace!("Trying a write from the backlog; items in backlog - {}", self.backlog.len());
            let status = {
                let buf = &mut self.backlog[0];
                let pos = buf.position() as usize;
                match try!(self.stream.async_write(&buf.get_ref()[pos..])) {
                    StreamStatus::Bytes(_) if buf.get_ref().len() == buf.position() as usize => {
                        trace!("Full frame written!");
                        WriteStatus::Full
                    },
                    StreamStatus::Bytes(sz) => {
                        trace!("Partial write: {} bytes", sz);
                        buf.set_position((pos + sz) as u64);
                        WriteStatus::Partial(sz)
                    },
                    _ => {
                        trace!("Write WOULDBLOCK");
                        WriteStatus::WouldBlock
                    },
                }
            };
            match status {
                WriteStatus::Full => {
                    // TODO A deque or even maybe a linked-list would be better than a vec
                    // for this type of thing...
                    self.backlog.remove(0);
                },
                WriteStatus::Partial(_) => {},
                WriteStatus::WouldBlock => {
                    self.set_writable(false);
                    break;
                },
            };
        }
        Ok(())
    }

    fn set_writable(&mut self, w: bool) {
        self.is_writable = w;
    }

    pub fn queue_frame(&mut self, frame: Vec<u8>) {
        trace!("Connection::queue_frame");
        self.backlog.push(Cursor::new(frame));
    }
}
