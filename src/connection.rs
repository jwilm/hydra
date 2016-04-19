//! Interactions with the I/O layer and the HTTP/2 state machine
use std::io::{self, Cursor};
use std::fmt;

use mio::{self, EventSet, EventLoop, TryRead, TryWrite};

use worker::{self, Worker};
use protocol::{self, Protocol, Http2};
use super::ConnectionError;
use super::ConnectionResult;

/// Type that receives events for a connection
pub trait Handler: Send + fmt::Debug + 'static {
    fn on_connection(&self, Handle);
    fn on_error(&self, ConnectionError);
    fn on_pong(&self);
}

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
    stream: mio::tcp::TcpStream,
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
    Partial,

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
/// To get a ConnectionHandle, a user must create a Hydra instance and establish a connection.
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

impl Handle {
    fn send(&self, msg: protocol::Msg) {
        self.tx.send(worker::Msg::Protocol(self.token, msg));
    }

    pub fn request<H>(&self, req: ::Request, handler: H)
        where H: protocol::StreamHandler
    {
        self.send(protocol::Msg::CreateStream(req, Box::new(handler)));
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
        self.on_error(ConnectionError::Timeout);
    }

    pub fn on_error(&self, err: ConnectionError) {
        self.handler.on_error(err);
    }

    pub fn new(token: mio::Token, stream: mio::tcp::TcpStream, handler: Box<Handler>) -> Connection {
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

    #[inline]
    pub fn stream(&self) -> &mio::tcp::TcpStream {
        &self.stream
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
                 events: EventSet) -> ConnectionResult<()>
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

        self.reregister(event_loop);

        Ok(())
    }

    fn reregister(&mut self, event_loop: &mut EventLoop<Worker>) {
        let mut flags = mio::EventSet::readable();
        if !self.backlog.is_empty() || self.protocol.wants_write() {
            flags = flags | mio::EventSet::writable();
        }

        let pollopt = mio::PollOpt::edge() | mio::PollOpt::oneshot();
        event_loop.reregister(self.stream(), self.token, flags, pollopt);
    }

    pub fn deregister(&mut self, event_loop: &mut EventLoop<Worker>) -> io::Result<()> {
        event_loop.deregister(self.stream())
    }

    pub fn notify(&mut self,
                  event_loop: &mut EventLoop<Worker>,
                  msg: protocol::Msg) -> ConnectionResult<()>
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
        self.reregister(event_loop);

        Ok(())
    }

    fn read(&mut self, event_loop: &mut EventLoop<Worker>) -> ConnectionResult<()> {
        // TODO Handle the case where the buffer isn't large enough to completely
        // exhaust the socket (since we're edge-triggered, we'd never get another
        // chance to read!)
        trace!("Handling read");
        match try!(self.stream.try_read_buf(&mut self.read_buf)) {
            Some(0) => {
                debug!("EOF");
                // TODO EOF; should close connection and any remaining streams
            },
            Some(n) => {
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
            None => {
                trace!("read WOULDBLOCK");
            },
        }

        Ok(())
    }

    fn try_write(&mut self, event_loop: &mut EventLoop<Worker>) -> ConnectionResult<()> {
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
                self.protocol.ready_write(conn_ref);
            }

            // Flush whatever the protocol might have added...
            // TODO Should we allow the protocol to queue more than once (this could hold
            // up the event loop if the protocol keeps adding stuff...)
            try!(self.write_backlog());
        }

        Ok(())
    }

    fn write_backlog(&mut self) -> ConnectionResult<()> {
        loop {
            if self.backlog.is_empty() {
                trace!("Backlog already empty.");
                return Ok(());
            }
            trace!("Trying a write from the backlog; items in backlog - {}", self.backlog.len());
            let status = {
                let buf = &mut self.backlog[0];
                match try!(self.stream.try_write_buf(buf)) {
                    Some(_) if buf.get_ref().len() == buf.position() as usize => {
                        trace!("Full frame written!");
                        WriteStatus::Full
                    },
                    Some(sz) => {
                        trace!("Partial write: {} bytes", sz);
                        WriteStatus::Partial
                    },
                    None => {
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
                WriteStatus::Partial => {},
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
