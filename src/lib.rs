//! Hydra - An evented HTTP/2 client library
//!
extern crate mio;
extern crate solicit;
extern crate hyper;
extern crate httparse;

#[macro_use]
extern crate log;

#[cfg(feature = "tls")]
extern crate openssl;

use std::marker::PhantomData;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::cell::Cell;
use std::net::{self, SocketAddr, ToSocketAddrs};
use std::str::FromStr;
use std::io;

// Would be cool if these got put into separate crates
pub use hyper::method::Method;
pub use hyper::status::{StatusCode, StatusClass};
pub use hyper::header::{self, Headers};

use solicit::http::session;

pub mod worker;
pub mod util;
pub mod protocol;
pub mod connection;

use worker::Worker;

pub use protocol::StreamHandler;
pub use connection::Handler as ConnectionHandler;

pub trait Connector: Send + 'static {}

use util::each_addr;

// pub struct TlsConnector;
// impl Connector for TlsConnector {}

pub struct PlaintextConnector;
impl Connector for PlaintextConnector {}

pub struct Config {
    pub threads: u8,
    pub connect_timeout_ms: u32,
    pub conns_per_thread: usize,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            threads: 8,
            connect_timeout_ms: u32,
            conns_per_thread: 1024,
        }
    }
}

/// Interface to evented HTTP/2 client worker pool
pub struct Hydra {
    workers: Vec<worker::Handle>,
    next: AtomicUsize,
}

impl Hydra {
    pub fn new(config: &Config) -> Hydra {
        let mut workers = Vec::new();
        for _ in 0..config.threads {
            workers.push(Worker::spawn(config));
        }

        Hydra {
            workers: workers,
            next: AtomicUsize::new(0),
        }
    }

    /// Connect to a host on plaintext transport
    ///
    /// FIXME U should be ToSocketAddrs
    pub fn connect<'a, U, H>(&'a self, addr: U, handler: H) -> ConnectionResult<Client<'a>>
        where U: ToSocketAddrs,
              H: ConnectionHandler,
    {
        // Parse the host param as a socket address
        let stream = try!(each_addr(addr, mio::tcp::TcpStream::connect));

        let next = self.next.fetch_add(1, Ordering::SeqCst);
        let worker = self.workers[next * self.workers.len()];
        let pending_connection = worker.connect(stream, Box::new(handler));

        worker.connect(stream, Box::new(handler));
    }

    #[cfg(feature = "tls")]
    pub fn connect_tls<'a, U, H>(&'a self, _url: U, _handler: H) -> Client<'a>
        where U: Into<String>,
              H: ConnectionHandler,
    {
        unimplemented!();
    }

    pub fn connect_with<'a, U, H, C>(&'a self, _url: U, _handler: H, _connector: C) -> Client<'a>
        where U: Into<String>,
              H: ConnectionHandler,
              C: Connector,
    {
        unimplemented!();
    }
}

pub struct Client<'a> {
    // connection: 
    _marker: PhantomData<&'a u8>
}

impl<'a> Client<'a> {
    pub fn request<H>(&self, _req: Request, handler: H) {
        let _handler = Box::new(handler);
        unimplemented!();
    }
}

#[derive(Debug)]
pub struct Request {
    method: Method,
    path: String,
    headers_only: bool,
}

impl Request {
    pub fn new<P, B>(method: Method, path: P) -> Request
        where P: Into<String>,
    {
        // TODO headers
        Request {
            method: method,
            path: path.into(),
            headers_only: false,
        }
    }

    pub fn request<P, B>(method: Method, path: P) -> Request
        where P: Into<String>,
    {
        // TODO headers - either here or via the handler. I'm inclined to say the handler.
        Request {
            method: method,
            path: path.into(),
            headers_only: true,
        }
    }
}

#[derive(Debug)]
pub struct Response;

pub enum ConnectionError {
    /// IO layer encountered an error.
    Io(io::Error),

    /// Timeout while connecting
    Timeout,

    /// The worker is closing and a connection can not be created.
    WorkerClosing,
}

impl From<io::Error> for ConnectionError {
    fn from(err: io::Error) -> ConnectionError {
        ConnectionError::Io(err)
    }
}

type ConnectionResult<T> = ::std::result::Result<T, ConnectionError>;

pub enum RequestError {
    Variant
}
