//! Misc utilities for Hydra
use std::io::{self, ErrorKind};
use std::net::{ToSocketAddrs, SocketAddr};

/// Threading utilities
pub mod thread {
    /// Like `thread::spawn`, but with a `name` argument
    pub fn spawn_named<F, T, S>(name: S, f: F) -> ::std::thread::JoinHandle<T>
        where F: FnOnce() -> T,
              F: Send + 'static,
              T: Send + 'static,
              S: Into<String>
    {
        ::std::thread::Builder::new().name(name.into()).spawn(f).expect("spawn thread")
    }
}

/// Run F for each addr
///
/// Does synchronous DNS lookup with getaddrinfo. This function was ripped from the std library
/// (it's private).
fn each_addr<A: ToSocketAddrs, F, T>(addr: A, mut f: F) -> io::Result<T>
    where F: FnMut(&SocketAddr) -> io::Result<T>
{
    let mut last_err = None;
    for addr in try!(addr.to_socket_addrs()) {
        match f(&addr) {
            Ok(l) => return Ok(l),
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap_or_else(|| {
        io::Error::new(ErrorKind::InvalidInput,
                   "could not resolve to any addresses")
    }))
}

