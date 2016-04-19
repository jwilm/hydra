//! Hydra - An evented HTTP/2 client library
//!
#![deny(unused_variables)]
#![deny(unused_imports)]
#![deny(dead_code)]
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
use std::net::ToSocketAddrs;
use std::io;

// Would be cool if these got put into separate crates
pub use hyper::method::Method;
pub use hyper::status::{StatusCode, StatusClass};
pub use hyper::header::{Headers};

pub mod worker;
pub mod util;
pub mod protocol;
pub mod connection;

mod header;

use worker::Worker;

pub use protocol::StreamHandler;
pub use protocol::Response;
pub use protocol::StreamDataState;

pub use connection::Handler as ConnectionHandler;
pub use ::solicit::http::ErrorCode as Http2ErrorCode;

pub trait Connector: Send + 'static {}

use util::each_addr;

// pub struct TlsConnector;
// impl Connector for TlsConnector {}

pub struct PlaintextConnector;
impl Connector for PlaintextConnector {}

pub struct Config {
    pub connect_timeout_ms: u64,
    pub conns_per_thread: usize,
    pub threads: u8,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            connect_timeout_ms: 60_000,
            conns_per_thread: 1_024,
            threads: 8,
        }
    }
}

#[derive(Debug)]
pub enum HydraError {
    Io(io::Error),
    Worker(worker::WorkerError),
}

impl ::std::error::Error for HydraError {
    fn cause(&self) -> Option<&::std::error::Error> {
        match *self {
            HydraError::Io(ref err) => Some(err),
            HydraError::Worker(ref err) => Some(err),
        }
    }

    fn description(&self) -> &str {
        match *self {
            HydraError::Io(_) => "encountered an I/O error",
            HydraError::Worker(ref err) => err.description(),
        }
    }
}

impl ::std::fmt::Display for HydraError {
    fn fmt(&self, f: &mut ::std::fmt::Formatter) -> ::std::fmt::Result {
        match *self {
            HydraError::Io(ref err) => write!(f, "Hydra encountered an I/O error: {}", err),
            HydraError::Worker(ref err) => write!(f, "Hydra worker encountered error: {}", err),
        }
    }
}

impl From<worker::WorkerError> for HydraError {
    fn from(val: worker::WorkerError) -> HydraError {
        HydraError::Worker(val)
    }
}

impl From<io::Error> for HydraError {
    fn from(val: io::Error) -> HydraError {
        HydraError::Io(val)
    }
}

pub type Result<T> = ::std::result::Result<T, HydraError>;

/// Interface to evented HTTP/2 client worker pool
pub struct Hydra {
    workers: Vec<worker::Handle>,
    next: AtomicUsize,
}

impl Hydra {
    pub fn new(config: &Config) -> Result<Hydra> {
        info!("spawning a hydra");

        let mut workers = Vec::new();
        for _ in 0..config.threads {
            workers.push(try!(Worker::spawn(config)));
        }

        Ok(Hydra {
            workers: workers,
            next: AtomicUsize::new(0),
        })
    }

    /// Connect to a host on plaintext transport
    ///
    /// FIXME U should be ToSocketAddrs
    pub fn connect<'a, U, H>(&'a self, addr: U, handler: H) -> Result<()>
        where U: ToSocketAddrs,
              H: ConnectionHandler,
    {
        debug!("connect");
        // Parse the host param as a socket address
        let stream = try!(each_addr(addr, mio::tcp::TcpStream::connect));

        let next = self.next.fetch_add(1, Ordering::SeqCst);
        let ref worker = self.workers[next % self.workers.len()];

        try!(worker.connect(stream, Box::new(handler)));

        Ok(())
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
    headers: Headers,
    path: String,
    headers_only: bool,
}

impl Request {
    pub fn new<P>(method: Method, path: P, headers: Headers) -> Request
        where P: Into<String>,
    {
        Request {
            method: method,
            path: path.into(),
            headers_only: false,
            headers: headers,
        }
    }

    pub fn new_headers_only<P>(method: Method, path: P, headers: Headers) -> Request
        where P: Into<String>,
    {
        Request {
            method: method,
            path: path.into(),
            headers_only: true,
            headers: headers,
        }
    }
}

#[derive(Debug)]
pub enum ConnectionError {
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
}

impl ::std::error::Error for ConnectionError {
    fn cause(&self) -> Option<&::std::error::Error> {
        match *self {
            ConnectionError::Io(ref err) => Some(err),
            ConnectionError::Http(ref err) => Some(err),
            _ => None,
        }
    }

    fn description(&self) -> &str {
        match *self {
            ConnectionError::Io(ref err) => err.description(),
            ConnectionError::Http(ref err) => err.description(),
            ConnectionError::Timeout => "timeout when attempting to connect",
            ConnectionError::WorkerClosing => "worker isn't accepting new connections",
            ConnectionError::WorkerFull => "worker cannot manage more connections",
        }
    }
}

impl ::std::fmt::Display for ConnectionError {
    fn fmt(&self, f: &mut ::std::fmt::Formatter) -> ::std::fmt::Result {
        match *self {
            ConnectionError::Io(ref err) => write!(f, "I/O error on connection: {}", err),
            ConnectionError::Http(ref err) => write!(f, "HTTP/2 error on connection: {}", err),
            ConnectionError::Timeout => write!(f, "Timeout during connect"),
            ConnectionError::WorkerClosing => write!(f, "Worker not accepting new connections"),
            ConnectionError::WorkerFull => write!(f, "Worker at connection capacity"),
        }
    }
}

impl From<io::Error> for ConnectionError {
    fn from(err: io::Error) -> ConnectionError {
        ConnectionError::Io(err)
    }
}

impl From<::solicit::http::HttpError> for ConnectionError {
    fn from(val: ::solicit::http::HttpError) -> ConnectionError {
        ConnectionError::Http(val)
    }
}

pub type ConnectionResult<T> = ::std::result::Result<T, ConnectionError>;

#[derive(Debug)]
pub enum RequestError {
    /// The remote end terminated this stream. No more data will arrive.
    Reset,

    /// request cannot be completed because the connection entered an error state
    Connection,

    /// Request cannot be completed because an error was returned from a stream handler method
    User,

    /// Received a GOAWAY frame on the connection where this request was being processed. This
    /// stream was not handled by the server.
    GoAwayUnprocessed(Http2ErrorCode),

    /// Received a GOAWAY frame on the connection handling this request; this stream was potentially
    /// processed by the server.
    GoAwayMaybeProcessed(Http2ErrorCode),
}

impl ::std::error::Error for RequestError {
    fn cause(&self) -> Option<&::std::error::Error> {
        None
    }

    fn description(&self) -> &str {
        match *self {
            RequestError::Reset => "stream reset by peer",
            RequestError::Connection => "connection encountered error",
            RequestError::User => "error during stream handler call",
            RequestError::GoAwayUnprocessed(_) => "GOAWAY frame received; stream not processed",
            RequestError::GoAwayMaybeProcessed(_) => {
                "GOAWAY frame received; stream maybe processed"
            },
        }
    }
}

impl ::std::fmt::Display for RequestError {
    fn fmt(&self, f: &mut ::std::fmt::Formatter) -> ::std::fmt::Result {
        try!(write!(f, "Request failed because "));
        match *self {
            RequestError::Reset => write!(f, "stream was reset by peer"),
            RequestError::Connection => write!(f, "connection encountered an error"),
            RequestError::User => write!(f, "an error was returned from a stream handler call"),
            RequestError::GoAwayUnprocessed(code) => {
                write!(f, "Received a GOAWAY, stream unprocessed; code: {:?}", code)
            },
            RequestError::GoAwayMaybeProcessed(code) => {
                write!(f, "Received a GOAWAY, stream maybe processed; code: {:?}", code)
            },
        }
    }
}

