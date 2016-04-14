extern crate hydra;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;

use hydra::{Method, Request, Hydra};

enum ConnectionMsg {
    Error(hydra::ConnectionError),
    Connected(hydra::worker::ConnectionHandle),
    Pong
}

struct ConnectHandler {
    tx: mpsc::Sender<ConnectionMsg>,
}

impl ConnectHandler {
    pub fn new(tx: mpsc::Sender<ConnectionMsg>) -> Self {
        ConnectHandler { tx: tx }
    }
}

impl hydra::ConnectionHandler for ConnectHandler {
    fn on_connection(&self, connection: hydra::worker::ConnectionHandle) {
        self.tx.send(ConnectionMsg::Connected(connection)).unwrap();
    }

    fn on_error(&self, err: hydra::ConnectionError) {
        self.tx.send(ConnectionMsg::Error(err)).unwrap();
    }

    fn on_pong(&self) {
        self.tx.send(ConnectionMsg::Pong).unwrap();
    }
}

fn response_collector() -> (StreamHandler, ResponseCollector) {
    let (tx, rx) = mpsc::channel();
    let counter = Arc::new(AtomicUsize::new(1));
    let handler = StreamHandler::new(tx, counter.clone());
    let collector = ResponseCollector::new(rx, counter);

    (handler, collector)
}

struct ResponseCollector {
    rx: mpsc::Receiver<Option<hydra::Response>>,
    counter: Arc<AtomicUsize>,
}

impl ResponseCollector {
    pub fn new(rx: mpsc::Receiver<Option<hydra::Response>>, counter: Arc<AtomicUsize>)
        -> ResponseCollector
    {
        ResponseCollector { rx: rx, counter: counter}
    }

    pub fn wait_all(&self) {
        while self.counter.load(Ordering::SeqCst) != 0 {
            let msg = self.rx.recv().unwrap();
            println!("resp: {:?}", msg);
        }
    }
}

struct StreamHandler {
    tx: mpsc::Sender<Option<hydra::Response>>,
    counter: Arc<AtomicUsize>,
}

impl Clone for StreamHandler {
    fn clone(&self) -> StreamHandler {
        self.counter.fetch_add(1, Ordering::SeqCst);
        StreamHandler {
            tx: self.tx.clone(),
            counter: self.counter.clone(),
        }
    }
}

impl StreamHandler {
    pub fn new(tx: mpsc::Sender<Option<hydra::Response>>,
               counter: Arc<AtomicUsize>) -> StreamHandler
    {
        StreamHandler { tx: tx, counter: counter, }
    }
}

impl hydra::StreamHandler for StreamHandler {
    fn on_error(&self, _err: hydra::RequestError) {
        self.counter.fetch_sub(1, Ordering::SeqCst);
        self.tx.send(None).unwrap();
    }

    fn on_response(&self, res: hydra::Response) {
        self.counter.fetch_sub(1, Ordering::SeqCst);
        self.tx.send(Some(res)).unwrap();
    }
}

#[test]
fn send_request() {
    let mut config = hydra::Config::default();
    config.threads = 1;

    let (tx, rx) = mpsc::channel();
    let handler = ConnectHandler::new(tx);

    let cluster = Hydra::new(&config);
    cluster.connect("http://http2bin.org", handler);

    // Wait for the connection.
    let client = match rx.recv().unwrap() {
        ConnectionMsg::Connected(conn) => conn,
        _ => panic!("Didn't recv Connected"),
    };

    let (req_handler, collector) = response_collector();

    let req = Request::new(Method::Get, "/get", "");
    client.request(req, req_handler.clone());

    let req = Request::new(Method::Get, "/get", "");
    client.request(req, req_handler.clone());

    let req = Request::new(Method::Get, "/get", "");
    client.request(req, req_handler);

    collector.wait_all();
}
