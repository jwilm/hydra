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
pub use hyper::header::{Headers};

use solicit::http::session;

pub mod worker;
pub mod util;
pub mod protocol;
pub mod connection;

mod header;

use worker::Worker;

pub use protocol::StreamHandler;
pub use protocol::Response;

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
            connect_timeout_ms: 60_000,
            conns_per_thread: 1_024,
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
        info!("spawning a hydra");

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
    pub fn connect<'a, U, H>(&'a self, addr: U, handler: H) -> ConnectionResult<()>
        where U: ToSocketAddrs,
              H: ConnectionHandler,
    {
        debug!("connect");
        // Parse the host param as a socket address
        let stream = try!(each_addr(addr, mio::tcp::TcpStream::connect));

        let next = self.next.fetch_add(1, Ordering::SeqCst);
        let ref worker = self.workers[next % self.workers.len()];

        worker.connect(stream, Box::new(handler));

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

pub type ConnectionResult<T> = ::std::result::Result<T, ConnectionError>;

pub enum RequestError {
    Variant
}
