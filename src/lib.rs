//! Hydra - An evented HTTP/2 client library
//!
extern crate mio;
extern crate solicit;
extern crate hyper;

#[macro_use]
extern crate log;

#[cfg(feature = "tls")]
extern crate openssl;

use std::marker::PhantomData;
use std::sync::atomic::AtomicUsize;
use std::cell::Cell;
use std::net::{self, SocketAddr};
use std::str::FromStr;

// Would be cool if these got put into separate crates
pub use hyper::method::Method;
pub use hyper::status::{StatusCode, StatusClass};
pub use hyper::header::{self, Headers};

mod worker;
mod util;

use worker::Worker;

pub trait ConnectionHandler: Send + 'static {
    fn on_connection(&self);
    fn on_error(&self, err: ConnectionError);
    fn on_pong(&self);
}

pub trait RequestHandler: Send + 'static {
    fn on_error(&self, err: RequestError);
    fn on_response(&self, res: Response);
}

pub trait Connector: Send + 'static {}

// pub struct TlsConnector;
// impl Connector for TlsConnector {}

pub struct PlaintextConnector;
impl Connector for PlaintextConnector {}

pub struct Config {
    pub threads: u8,
    pub connect_timeout: u32,
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
    pub fn connect<'a, U, H>(&'a self, host: U, handler: H) -> ConnectionResult<Client<'a>, Error>
        where U: AsRef<str>,
              H: ConnectionHandler,
    {
        // Parse the host param as a socket address
        let addr = try!(ParseSocketAddr(host.as_ref()));
        let stream = try!(mio::tcp::TcpStream::connect(&addr));

        let next = self.next.fetch_add(1, Ordering::SeqCst);
        let worker = workers[next * workers.len()];
        let pending_connection = worker.connect(stream, Box::new(handler));

        // mio::tcp::TcpStream::connect
        unimplemented!();
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

pub struct Request {
    _method: Method,
    _path: String,
    _body: Vec<u8>,
}

impl Request {
    pub fn new<P, B>(method: Method, path: P, body: B) -> Request
        where P: Into<String>,
              B: Into<Vec<u8>>,
    {
        Request {
            _method: method,
            _path: path.into(),
            _body: body.into(),
        }
    }
}

#[derive(Debug)]
pub struct Response;

pub enum ConnectionError {
    /// Error parsing a str as socket address
    ParseSocketAddr(net::AddrParseError),
}

type ConnectionResult<T> = ::std::result::Result<T, ConnectionError>;

pub enum RequestError {
    Variant
}
