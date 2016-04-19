//! Evented HTTP/2 client threads supporting multiple connections
//!
//! Workers contain a mio event loop, per connection HTTP/2 state (stream management, flow control,
//! etc), and a state object accessible from the handle.
use std::thread::JoinHandle;

use mio::tcp::TcpStream;
use mio::util::Slab;
use mio::{self, Token, EventSet, EventLoop};

use protocol::{self, Protocol};
use connection::{self, Connection};
use util::thread;

use super::ConnectionError;

/// Messages sent to the worker
#[derive(Debug)]
pub enum Msg {
    /// Request to establish a new connection.
    CreatePlaintextConnection(TcpStream, Box<connection::Handler>),

    /// Send a message to a client on this thread
    Protocol(mio::Token, protocol::Msg),

    /// Worker should stop creating new streams, finish processing active streams, and terminate.
    Terminate,
}

pub enum Timer {
    /// Connect timed out
    Connect(Token),
}

/// The Worker lives on its own thread managing I/O for HTTP/2 requests.
///
/// The Worker struct resides on its own thread. Interacting with the worker is completed through
/// the handle.
pub struct Worker {
    /// Connect timeout
    connect_timeout_ms: u32,

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
    /// Notifier handle for the worker's event loop.
    tx: mio::Sender<Msg>,

    /// Handle to the worker thread.
    ///
    /// The thread handle is stored in an option so the thread can be joined without consuming the
    /// handle.
    thread: Option<JoinHandle<()>>,
}

// -------------------------------------------------------------------------------------------------
// Handle impls
// -------------------------------------------------------------------------------------------------

impl Handle {
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
    pub fn connect(&self, stream: TcpStream, handler: Box<connection::Handler>) {
        self.tx.send(Msg::CreatePlaintextConnection(stream, handler));
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
        trace!("spawning a hydra worker");

        let mut worker = Worker {
            connect_timeout_ms: config.connect_timeout_ms,
            connections: Slab::new(config.conns_per_thread),
            closing: false,
        };

        let mut event_loop = EventLoop::new().expect("create event loop");
        let sender = event_loop.channel();

        let join_handle = thread::spawn_named("Hydra Worker", move || {
            worker.run(&mut event_loop);
        });

        Handle {
            tx: sender,
            thread: Some(join_handle),
        }
    }

    /// Create a new connection given the handler and stream
    ///
    /// A new `Connection` is created, the stream is registered with the event loop, and the
    /// connection is placed in the connections slab.
    fn add_connection(&mut self,
                      event_loop: &mut EventLoop<Worker>,
                      stream: TcpStream,
                      handler: Box<connection::Handler>)
    {
        self.connections.insert_with(|token| {
            let events = EventSet::readable() | EventSet::writable();
            let pollopt = mio::PollOpt::edge() | mio::PollOpt::oneshot();
            event_loop.register(&stream, token, events, pollopt);

            let mut conn = Connection::new(token, stream, handler);
            conn.initialize(event_loop);
            conn
        });
    }

    /// Main function for the worker thread
    ///
    /// Runs the event loop indefinitely until shutdown is called.
    pub fn run(&mut self, event_loop: &mut EventLoop<Worker>) {
        info!("worker running");
        event_loop.run(self);

        // TODO remaining connections need to finish their work, close out streams, etc. Raise a
        // flag on each of the connections that shutdown is happening so new requests are rejected.
        self.closing = true;
        info!("worker terminated");
    }
}

impl mio::Handler for Worker {
    type Timeout = Timer;
    type Message = Msg;

    fn ready(&mut self,
             event_loop: &mut EventLoop<Worker>,
             token: Token,
             events: EventSet)
    {
        if let Err(err) = self.connections[token].ready(event_loop, events) {
            let mut connection = self.connections.remove(token).expect("connection in slab");
            connection.deregister(event_loop);
            connection.on_error(err);
        }
    }

    fn notify(&mut self, event_loop: &mut EventLoop<Worker>, msg: Msg) {
        debug!("worker got msg: {:?}", msg);
        match msg {
            Msg::CreatePlaintextConnection(stream, handler) => {
                if self.closing {
                    handler.on_error(ConnectionError::WorkerClosing);
                } else {
                    self.add_connection(event_loop, stream, handler);
                }
            },
            Msg::Protocol(token, protocol_msg) => {
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

    fn timeout(&mut self, event_loop: &mut EventLoop<Worker>, timeout: Timer) {
        match timeout {
            Timer::Connect(token) => {
                // If a ConnectTimeout arrives, that means the connection has not been established.
                // Remove the connection and run the timeout handler.
                if let Some(conn) = self.connections.remove(token) {
                    event_loop.deregister(conn.stream());
                    conn.on_connect_timeout();
                }
            }
        }
        unimplemented!();
    }
}
