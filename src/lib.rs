//! Hydra - An evented HTTP/2 client library
//!
extern crate mio;
extern crate solicit;
extern crate hyper;

use std::marker::PhantomData;

// Would be cool if these got put into separate crates
pub use hyper::method::Method;
pub use hyper::status::{StatusCode, StatusClass};
pub use hyper::header::{self, Headers};

#[cfg(feature = "tls")]
extern crate openssl;

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

pub struct Config {
    pub threads: u8,
}

impl Default for Config {
    fn default() -> Config {
        Config { threads: 8 }
    }
}

pub struct Hydra<H: ConnectionHandler> {
    _handler: H
}

impl<H> Hydra<H>
    where H: ConnectionHandler
{
    pub fn new(_config: &Config) -> Hydra<H> {
        unimplemented!();
    }

    pub fn connect<'a, U>(&'a self, _url: U, _handler: H) -> Client<'a>
        where U: Into<String>
    {
        unimplemented!();
    }

    #[cfg(feature = "tls")]
    pub fn connect_tls<'a, U>(&'a self, _url: U, _handler: H) -> Client<'a>
        where U: Into<String>
    {
        unimplemented!();
    }

    pub fn connect_with<'a, U, C>(&'a self, _url: U, _handler: H, _connector: C) -> Client<'a>
        where C: Connector,
              U: Into<String>
    {
        unimplemented!();
    }
}

pub struct Client<'a> {
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
    Variant
}
pub enum RequestError {
    Variant
}
