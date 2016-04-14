//! Evented HTTP/2 client threads supporting multiple connections
//!
//! Workers contain a mio event loop, per connection HTTP/2 state (stream management, flow control,
//! etc), and a state object accessible from the handle.
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::io::Cursor;

use mio::tcp::TcpStream;
use mio::util::Slab;
use mio::{self, Token, EventSet, EventLoop};

use protocol::{self, Protocol, Http2};
use util::thread;

/// Messages sent to the worker
enum Msg {
    /// Request to establish a new connection.
    CreatePlaintextConnection(TcpStream, Box<::ConnectionHandler>),

    /// Send a message to a client on this thread
    Protocol(mio::Token, protocol::Msg),

    /// Worker should stop creating new streams, finish processing active streams, and terminate.
    Terminate,
}

/// Notices wake up the worker event loop
enum Notice {
    /// There are pending messaages
    Msg,
}

enum Timeout {
    /// Connect timed out
    Connect(Token),
}

pub trait DispatchConnectionEvent {
    fn on_connection(&self);
    fn on_pong(&self);
}

/// Contains state relevant for managing a worker.
pub struct Info {
    /// Number of active worker connections
    ///
    /// Workers have a maximum number of connections they can handle. Tracking active_connections is
    /// necessary so that higher level code can avoid requesting a new connection when the pool is
    /// full.
    active_connections: AtomicUsize,
}

struct Connection {
    token: Token,
    stream: TcpStream,
    backlog: Vec<Cursor<Vec<u8>>>,
    is_writable: bool,
    protocol: Http2,
    read_buf: Vec<u8>,
    handler: Box<ConnectionHandler>,
}

/// A projection of the Connection type for use within the worker thread.
struct ConnectionRef<'a> {
    backlog: &'a mut Vec<Cursor<Vec<u8>>>,
    handler: &'a ConnectionHandler,
    event_loop: &'a mut EventLoop<Worker>,
    token: Token,
    is_writable: bool,
}

/// Handle for a connection usable from other threads
///
/// To get a ConnectionHandle, a user must create a Hydra instance and establish a connection.
#[derive(Debug, Clone)]
pub struct ConnectionHandle {
    // How many streams are currently active.
    //
    // This value is exposed to the client so they can hold on to the handle as long as there are
    // still requests active. Hmm. Maybe the event loop should just be smart enough to not close
    // while there's still activity?
    // active_streams: Arc<AtomicUsize>,

    // Number of requests queued in the connection
    //
    // Should only be nonzero when active_streams matches max_concurrent_streams.
    // queued_requests: Arc<AtomicUsize>,

    /// Sender to event loop where connection is running.
    tx: mio::Sender<Msg>,

    /// Token for the connection on its corresponding thread
    token: mio::Token,
}

impl ConnectionHandle {
    fn send(msg: protocol::Msg) {
        self.tx.send(Msg::Protocol(self.token, msg));
    }

    fn request<H>(req: ::Request, handler: H)
        where H: ConnectionHandler
    {
        self.send(protocol::Msg::CreateStream(req, Box::new(handler)));
    }
}

/// The Worker lives on its own thread managing I/O for HTTP/2 requests.
///
/// The Worker struct resides on its own thread. Interacting with the worker is completed through
/// the handle.
pub struct Worker {
    /// Receiver for messages from the Handle
    rx: mpsc::Receiver<Msg>,

    /// Shared info about this worker
    info: Arc<Info>,

    /// Connect timeout
    connect_timeout: u32,

    /// Connections
    connections: Slab<Connection>,

    /// True when the worker is cleaning up and shutting down
    closing: bool,
}

/// Handle to a worker
///
/// The Handle is the primary interface for interacting with an HTTP/2 worker. High level worker
/// state, `worker::Info`, can be accessed from here. The handle is also used to request connection
/// creation and worker shutdown.
pub struct Handle {
    /// Sender for delivering messages to the worker
    tx: mpsc::Sender<Msg>,

    /// Notifier handle for the worker's event loop.
    notifier: mio::Sender<Msg>,

    /// Handle to the worker thread.
    ///
    /// The thread handle is stored in an option so the thread can be joined without consuming the
    /// handle.
    thread: Option<JoinHandle<()>>,

    /// Info for managed worker
    info: Arc<Info>,
}

// -------------------------------------------------------------------------------------------------
// Handle impls
// -------------------------------------------------------------------------------------------------

impl Handle {
    /// Get worker info
    pub fn info(&self) -> &Info {
        &*self.info
    }

    /// Terminate the worker
    ///
    /// Any errors occurring during join are logged. Terminate may be called multiple times, but it
    /// will have no effect after the first.
    pub fn terminate(&mut self) {
        if let Some(join_handle) = self.thread.take() {
            if let Err(err) = join_handle.join() {
                error!("Joining worker: {:?}", err);
            }
        }
    }

    /// Worker should create a new connection with the given TcpStream
    pub fn connect(&self,
                   stream: TcpStream,
                   handler: Box<ConnectionHandler>) -> Result<(), SendError<Msg>>
    {
        self.tx.send(Msg::CreatePlaintextConnection(stream, handler))
    }
}

// -------------------------------------------------------------------------------------------------
// Info impls
// -------------------------------------------------------------------------------------------------

impl Info {
    /// Create a new Info struct.
    pub fn new() -> Info {
        Default::default()
    }

    /// Number of active connections for the worker
    pub fn active_connections(&self) -> usize {
        self.active_connections.load(Ordering::SeqCst)
    }
}

impl Default for Info {
    fn default() -> Info {
        Info {
            active_connections: AtomicUsize::new(0),
        }
    }
}

// -------------------------------------------------------------------------------------------------
// Worker impls
// -------------------------------------------------------------------------------------------------

impl Worker {
    /// Create a new worker
    ///
    /// Spawns a new thread with a worker using config. A `Handle` for the Worker thread is retured.
    pub fn spawn(config: &::Config) -> Handle {
        let info = Arc::new(Info::new());
        let (tx, rx) = mpsc::channel();

        let mut worker = Worker {
            connect_timeout: config.connect_timeout,
            info: info.clone(),
            rx: rx,
            connections: Slab::new(config.conns_per_thread),
            closing: false,
        };

        let mut event_loop = EventLoop::new().expect("create event loop");
        let sender = event_loop.channel();

        let join_handle = thread::spawn_named("Hydra Worker", move || {
            worker.run(&mut event_loop);
        });

        Handle {
            info: info,
            tx: tx,
            thread: Some(join_handle),
            notifier: sender,
        }
    }

    fn add_connection(&mut self,
                      event_loop: &mut EventLoop<Worker>,
                      stream: TcpStream,
                      handler: Box<ConnectionHandler>)
    {
        self.connections.insert_with(|token| {
            // TODO create a connection
            let conn = Connection::new(token, stream, handler);
            conn
        });
    }

    /// Pull messages from the mpsc channel and process them
    fn process_messages(&mut self, event_loop: &mut EventLoop<Worker>) {
        loop {
            if let Ok(msg) = self.rx.try_recv() {
                match msg {
                    Msg::CreatePlaintextConnection(stream, handler) => {
                        if self.closing {
                            handler.on_error(CreateConnectionError::WorkerClosing);
                        } else {
                            self.add_connection(event_loop, stream, handler);
                        }
                    },
                    Msg::Protocol(ref protocol_msg) => {
                        self.connections[token].notify(event_loop, protocol_msg);
                    },
                    Msg::Terminate => {
                        if !self.closing {
                            self.closing = true;
                            event_loop.shutdown();
                        }
                    }
                }
            }
        }
    }

    /// Main function for the worker thread
    ///
    /// Runs the event loop indefinitely until shutdown is called.
    pub fn run(&mut self, event_loop: &mut EventLoop<Worker>) {
        event_loop.run(&mut self);

        // TODO remaining connections need to finish their work, close out streams, etc. Raise a
        // flag on each of the connections that shutdown is happening so new requests are rejected.
        self.closing = true;
    }
}

impl mio::Handler for Worker {
    type Timeout = Timeout;
    type Message = Notice;

    fn ready(&mut self,
             event_loop: &mut EventLoop<Worker>,
             token: Token,
             events: EventSet)
    {
        self.connections[token].ready(event_loop, events);
    }

    fn notify(&mut self, event_loop: &mut EventLoop<Worker>, _msg: Notice) {
        self.process_messages(event_loop);
    }

    fn timeout(&mut self, event_loop: &mut EventLoop<Worker>, timeout: Timeout) {
        match timeout {
            Timeout::ConnectTimeout(token) => {
                // If a ConnectTimeout arrives, that means the connection has not been established.
                // Remove the connection and run the timeout handler.
                if let Some(conn) = self.connections.remove(token) {
                    event_loop.deregister(conn.stream);
                    conn.handle_connect_timeout();
                }
            }
        }
        unimplemented!();
    }
}

/// ------------------------------------------------------------------------------------------------
/// Connection impls
/// ------------------------------------------------------------------------------------------------

impl Connection {
    fn new(token: Token, stream: TcpStream, handler: Box<ConnectionHandler>) -> Connection {
        Connection {
            token: token,
            stream: stream,
            backlog: Vec::new(),
            is_writable: false,
            protocol: proto,
            buf: Vec::with_capacity(4096),
            handler: handler,
        }
    }

    fn ready(&mut self, event_loop: &mut EventLoop<Worker>, events: EventSet) {
        trace!("Connection ready: token={:?}; events={:?}", self.token, events);
        if events.is_readable() {
            self.read(event_loop);
        }
        if events.is_writable() {
            self.set_writable(true);
            // Whenever the connection becomes writable, we try a write.
            self.try_write(event_loop).unwrap();
        }
    }

    fn notify(&mut self, event_loop: &mut EventLoop<Worker>, msg: protocol::Msg) {
        trace!("Connection notified: token={:?}; msg={:?}", self.token, msg);

        let conn_ref = ConnectionRef {
            event_loop: event_loop,
            backlog: &mut self.backlog,
            handle: ConnectionHandle::new(
                DispatcherHandle::new_for_loop(event_loop),
                self.token),
            writable: self.is_writable,
        };

        self.protocol.notify(msg, conn);
    }

    fn read(&mut self, event_loop: &mut EventLoop<Worker>) {
        // TODO Handle the case where the buffer isn't large enough to completely
        // exhaust the socket (since we're edge-triggered, we'd never get another
        // chance to read!)
        trace!("Handling read");
        match self.stream.try_read_buf(&mut self.read_buf) {
            Ok(Some(0)) => {
                debug!("EOF");
                // EOF
            },
            Ok(Some(n)) => {
                debug!("read {} bytes", n);
                let conn_ref = ConnectionRef {
                    event_loop: event_loop,
                    token: self.token,
                    backlog: &mut self.backlog,
                    is_writable: self.is_writable,
                    handler: &*self.handler,
                };
                let buf = &self.read_buf;
                let consumed = self.protocol.on_data(buf, conn_ref).unwrap();
                trace!("Draining... {}/{}", consumed, self.read_buf.len());

                // TODO Would a circular buffer be better than draining here...?
                // Though it might not be ... there shouldn't be that many elems
                // to copy to the front usually... this would give an advantage
                // to later reads as there will never be a situation where a mini
                // read needs to be done because we're about to wrap around in the
                // buffer ... on the other hand, a mini read every-so-often might
                // even be okay? If it eliminates copies...
                self.read_buf.drain(..consumed);

                trace!("Done.");
            },
            Ok(None) => {
                trace!("read WOULDBLOCK");
            },
            Err(e) => {
                // TODO FIXME
                panic!("got an error trying to read; err={:?}", e);
            },
        };
    }

    fn try_write(&mut self, event_loop: &mut EventLoop<Worker>) -> Result<(), ()> {
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
                let conn_ref = ConnectionRef {
                    event_loop: event_loop,
                    token: self.token,
                    backlog: &mut self.backlog,
                    is_writable: self.is_writable,
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

    fn write_backlog(&mut self) -> Result<(), ()> {
        loop {
            if self.backlog.is_empty() {
                trace!("Backlog already empty.");
                return Ok(());
            }
            trace!("Trying a write from the backlog; items in backlog - {}", self.backlog.len());
            let status = {
                let buf = &mut self.backlog[0];
                match self.stream.try_write_buf(buf) {
                    Ok(Some(_)) if buf.get_ref().len() == buf.position() as usize => {
                        trace!("Full frame written!");
                        trace!("{:?}", buf);
                        WriteStatus::Full
                    },
                    Ok(Some(sz)) => {
                        trace!("Partial write: {} bytes", sz);
                        WriteStatus::Partial
                    },
                    Ok(None) => {
                        trace!("Write WOULDBLOCK");
                        WriteStatus::WouldBlock
                    },
                    Err(e) => {
                        panic!("Error writing! {:?}", e);
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
        self.backlog.push(Cursor::new(frame));
    }
}

/// ------------------------------------------------------------------------------------------------
/// ConnectionRef impls
/// ------------------------------------------------------------------------------------------------

impl DispatchConnectionEvent for ConnectionRef {
    #[inline]
    fn on_connection<S: Settings>(&self) {
        let handle = ConnectionHandle {
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
