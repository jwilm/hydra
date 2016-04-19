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

pub use ::solicit::http::ErrorCode as Http2ErrorCode;

// Would be cool if these got put into separate crates
pub use hyper::method::Method;
pub use hyper::status::{StatusCode, StatusClass};

pub mod worker;
pub mod util;
pub mod protocol;
pub mod connection;
pub mod request;

mod header;

use worker::Worker;
use util::each_addr;

pub use protocol::StreamDataState;

pub use connection::Handler as ConnectionHandler;

pub trait Connector: Send + 'static {}

pub mod prelude {
    pub use Hydra;
    pub use Method;
    pub use StatusCode;
    pub use StreamDataState;
    pub use connection;
    pub use header::Headers;
    pub use request::Request;
    pub use request::Response;
    pub use request;
}


pub struct PlaintextConnector;
impl Connector for PlaintextConnector {}

/// Configuration for Hydra instance
pub struct Config {
    /// Timeout in milliseconds for connect calls. Connect error handler will be called with the
    /// Timeout error if a timeout occurs.
    pub connect_timeout_ms: u64,

    /// Number of connections per worker
    pub conns_per_thread: usize,

    /// Number of worker to spawn
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
    Worker(worker::Error),
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

impl From<worker::Error> for HydraError {
    fn from(val: worker::Error) -> HydraError {
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
    pub fn request<H>(&self, _req: request::Request, handler: H) {
        let _handler = Box::new(handler);
        unimplemented!();
    }
}

