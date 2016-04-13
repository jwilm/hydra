extern crate hydra;
extern crate env_logger;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;

use hydra::{Method, Request, Hydra};
use hydra::{connection, protocol};
use hydra::Headers;

enum ConnectionMsg {
    Error(hydra::ConnectionError),
    Connected(connection::Handle),
    Pong
}

#[derive(Debug)]
struct ConnectHandler {
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

struct ResponseCollector {
    rx: mpsc::Receiver<Option<Vec<u8>>>,
    tx: mpsc::Sender<Option<Vec<u8>>>,
    counter: Arc<AtomicUsize>,
}

impl ResponseCollector {
    pub fn new() -> ResponseCollector {
        let (tx, rx) = mpsc::channel();
        ResponseCollector {
            rx: rx,
            tx: tx,
            counter: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn new_stream_handler(&self) -> StreamHandler {
        self.counter.fetch_add(1, Ordering::Relaxed);

        StreamHandler::new(self.tx.clone(), self.counter.clone())
    }

    pub fn wait_all(&self) {
        while self.counter.load(Ordering::SeqCst) != 0 {
            let msg = self.rx.recv().unwrap();
            println!("resp: {:?}", msg);
        }
    }
}

#[derive(Debug)]
struct StreamHandler {
    tx: mpsc::Sender<Option<Vec<u8>>>,
    res: Vec<u8>,
    counter: Arc<AtomicUsize>,
}

impl StreamHandler {
    pub fn new(tx: mpsc::Sender<Option<Vec<u8>>>,
               counter: Arc<AtomicUsize>) -> StreamHandler
    {
        StreamHandler {
            tx: tx,
            counter: counter,
            res: Vec::new()
        }
    }
}

impl hydra::StreamHandler for StreamHandler {
    fn on_error(&mut self, _err: hydra::RequestError) {
        self.counter.fetch_sub(1, Ordering::SeqCst);
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
        self.counter.fetch_sub(1, Ordering::SeqCst);

        let mut res = Vec::new();
        ::std::mem::swap(&mut self.res, &mut res);

        self.tx.send(Some(res)).unwrap();
    }
}

#[test]
fn send_request() {
    env_logger::init().ok();

    let mut config = hydra::Config::default();
    config.threads = 1;

    let (tx, rx) = mpsc::channel();
    let handler = ConnectHandler::new(tx);

    let cluster = Hydra::new(&config);
    cluster.connect("http2bin.org:80", handler).unwrap();

    // Wait for the connection.
    let client = match rx.recv().unwrap() {
        ConnectionMsg::Connected(conn) => conn,
        _ => panic!("Didn't recv Connected"),
    };

    let collector = ResponseCollector::new();

    for _ in 0..3 {
        let req = Request::new_headers_only(Method::Get, "/get");
        client.request(req, collector.new_stream_handler());
    }

    collector.wait_all();
}
