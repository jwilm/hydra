use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;

use hydra::{self, Method, Request, connection, protocol, Headers};

/// Messages sent by ConnectHandler
pub enum ConnectionMsg {
    Error(hydra::ConnectionError),
    Connected(connection::Handle),
    Pong
}

/// Handles connection related events by implementing connection::Handler
///
/// All events are sent on a channel.
#[derive(Debug)]
pub struct ConnectHandler {
    tx: mpsc::Sender<ConnectionMsg>,
}

impl ConnectHandler {
    pub fn new(tx: mpsc::Sender<ConnectionMsg>) -> Self {
        ConnectHandler { tx: tx }
    }
}

impl connection::Handler for ConnectHandler {
    fn on_connection(&self, connection: connection::Handle) {
        self.tx.send(ConnectionMsg::Connected(connection)).unwrap();
    }

    fn on_error(&self, err: hydra::ConnectionError) {
        self.tx.send(ConnectionMsg::Error(err)).unwrap();
    }

    fn on_pong(&self) {
        self.tx.send(ConnectionMsg::Pong).unwrap();
    }
}

/// Helper for receiving multiple responses
///
/// To use the collector, first create one with `new`, and call `new_stream_handler` for every
/// StreamHandler that is needed (one per request). The `wait_all` function is used to block until
/// all of the streams have been resolved through any means (nominally, error, etc).
pub struct ResponseCollector {
    rx: mpsc::Receiver<Option<Vec<u8>>>,
    tx: mpsc::Sender<Option<Vec<u8>>>,
    counter: AtomicUsize,
}

impl Default for ResponseCollector {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        ResponseCollector {
            rx: rx,
            tx: tx,
            counter: AtomicUsize::new(0),
        }
    }
}

impl ResponseCollector {
    /// Builds a ResponseCollector
    pub fn new() -> ResponseCollector {
        Default::default()
    }

    /// Get a stream handler
    ///
    /// Returns a StreamHandler and increments the number of responses `wait_all` expects
    pub fn new_stream_handler(&self) -> StreamHandler {
        self.counter.fetch_add(1, Ordering::Relaxed);

        StreamHandler::new(self.tx.clone())
    }

    /// Block until a response is received for every stream handler.
    ///
    /// TODO would be nice if this could time out; it currently blocks indefinitely.
    pub fn wait_all(&self) {
        while self.counter.load(Ordering::SeqCst) != 0 {
            let msg = self.rx.recv().unwrap();
            self.counter.fetch_sub(1, Ordering::SeqCst);
            println!("resp: {:?}", msg);
        }
    }
}

/// Implementor of hydra::StreamHandler; receives stream events
///
/// When an error occurs or the stream terminates normally, an event is sent on the channel passed
/// to `new`.
#[derive(Debug)]
pub struct StreamHandler {
    tx: mpsc::Sender<Option<Vec<u8>>>,
    res: Vec<u8>,
}

impl StreamHandler {
    pub fn new(tx: mpsc::Sender<Option<Vec<u8>>>) -> StreamHandler
    {
        StreamHandler {
            tx: tx,
            res: Vec::new()
        }
    }
}

impl hydra::StreamHandler for StreamHandler {
    fn on_error(&mut self, _err: hydra::RequestError) {
        self.tx.send(None).unwrap();
    }

    fn on_response_data(&mut self, bytes: &[u8]) {
        self.res.extend_from_slice(bytes);
    }

    fn get_data_chunk(&mut self, buf: &mut [u8])
        -> Result<protocol::StreamDataChunk, protocol::StreamDataError>
    {
        unimplemented!();
    }

    /// Response headers are available
    fn on_response_headers(&mut self, res: Headers) {
        println!("headers: {:?}", res);
    }

    /// Called when the stream is closed (complete)
    fn on_close(&mut self) {
        let mut res = Vec::new();
        ::std::mem::swap(&mut self.res, &mut res);

        self.tx.send(Some(res)).unwrap();
    }
}
