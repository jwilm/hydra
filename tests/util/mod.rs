//! Helpers for writing Hydra tests
use std::fmt;
use std::io::Cursor;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;

use hydra::{self, Method, Request, connection, protocol, Headers};
use hydra::StatusCode;


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
pub struct ResponseCollector<S> {
    rx: mpsc::Receiver<Option<BufferedResponse>>,
    tx: mpsc::Sender<Option<BufferedResponse>>,
    counter: AtomicUsize,
    responses: Vec<BufferedResponse>,
    _marker: PhantomData<S>
}

pub struct BufferedResponse {
    pub response: hydra::Response,
    pub body: Vec<u8>,
}

impl<S> Default for ResponseCollector<S> {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        ResponseCollector {
            rx: rx,
            tx: tx,
            counter: AtomicUsize::new(0),
            responses: Vec::new(),
            _marker: PhantomData
        }
    }
}

impl<S: Collectable> ResponseCollector<S> {
    /// Builds a ResponseCollector
    pub fn new() -> ResponseCollector<S> {
        Default::default()
    }

    /// Get a stream handler
    ///
    /// Returns a StreamHandler and increments the number of responses `wait_all` expects
    pub fn new_stream_handler(&self) -> S {
        self.counter.fetch_add(1, Ordering::Relaxed);

        S::new(self.tx.clone())
    }

    /// Block until a response is received for every stream handler.
    ///
    /// TODO would be nice if this could time out; it currently blocks indefinitely.
    pub fn wait_all(&mut self) {
        while self.counter.load(Ordering::SeqCst) != 0 {
            let msg = self.rx.recv().unwrap();
            self.counter.fetch_sub(1, Ordering::SeqCst);
            if let Some(buffered) = msg {
                self.responses.push(buffered);
            } else {
                panic!("unexpected msg");
            }
        }
    }

    /// Run a checker against the buffered responses.
    pub fn check_responses<C>(&self, _: C)
        where C: CheckResponse,
    {
        for response in &self.responses {
            C::check(response);
        }
    }
}

pub trait Collectable: hydra::StreamHandler {
    fn new(tx: mpsc::Sender<Option<BufferedResponse>>) -> Self;
}

/// Implementor of hydra::StreamHandler; receives stream events
///
/// When an error occurs or the stream terminates normally, an event is sent on the channel passed
/// to `new`.
#[derive(Debug)]
pub struct HeadersOnlyHandler {
    tx: mpsc::Sender<Option<BufferedResponse>>,
    // outgoing: Cursor<Vec<u8>>,
    body: Vec<u8>,
    response: Option<hydra::Response>
}

impl fmt::Display for BufferedResponse {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Response\n\
                   ========\n\
                   {}\n\
                   {:?}\n", self.response.headers, ::std::str::from_utf8(&self.body[..]))
    }
}

impl Collectable for HeadersOnlyHandler {
    fn new(tx: mpsc::Sender<Option<BufferedResponse>>) -> HeadersOnlyHandler {
        HeadersOnlyHandler {
            tx: tx,
            body: Vec::new(),
            response: None,
        }
    }
}

impl hydra::StreamHandler for HeadersOnlyHandler {
    fn on_error(&mut self, _err: hydra::RequestError) {
        self.tx.send(None).unwrap();
    }

    fn on_response_data(&mut self, bytes: &[u8]) {
        self.body.extend_from_slice(bytes);
    }

    fn get_data_chunk(&mut self, buf: &mut [u8])
        -> Result<protocol::StreamDataChunk, protocol::StreamDataError>
    {
        unimplemented!();
    }

    /// Response headers are available
    fn on_response(&mut self, res: hydra::Response) {
        self.response = Some(res);
    }

    /// Called when the stream is closed (complete)
    fn on_close(&mut self) {
        let mut res = Vec::new();
        ::std::mem::swap(&mut self.body, &mut res);

        let response = self.response.take().unwrap();

        self.tx.send(Some(BufferedResponse {
            response: response,
            body: res,
        })).unwrap();
    }
}

/// Trait for checking responses; consumed by Collector.check_responses
///
/// The easiest way to get a type that implements CheckResponse is to use the `response_spec!`
/// macro.
pub trait CheckResponse {
    fn check(&BufferedResponse);
}

#[macro_export]
macro_rules! response_spec {
    {
        type_name => $type_name:ident,
        status => $status:expr,
        body_contains => [$( $contains_str:expr ),*],
        headers => {
            $($header_name:expr => $header_value:expr),*
        }
    } => {
        struct $type_name;

        impl $crate::util::CheckResponse for $type_name {
            fn check(res: &$crate::util::BufferedResponse) {
                // Compare the status.
                assert_eq!(res.response.status, $status);

                // Get a utf8 body
                let body = ::std::str::from_utf8(&res.body[..]).expect("utf8 body");

                // Print it for debugging purposes. Will show up in case of panic or --nocapture.
                println!("body: {}", body);

                // Check each body_contains
                $(
                    assert!(body.contains($contains_str));
                )*

                // Check for header equivalence
                let headers = &res.response.headers;

                $(
                    {
                        // Types aren't used directly here since headers aren't required to impl Eq
                        // or PartialEq. If that ever changes, accept a $ty param instead of $expr.
                        let raw = headers.get_raw($header_name).unwrap();
                        let utf8 = ::std::str::from_utf8(&raw[0][..]).unwrap();
                        assert!(utf8.contains($header_value));
                    }
                )*
            }
        }
    }
}

fn response_spec_test_response() -> BufferedResponse {
    let mut headers = ::hyper::header::Headers::new();
    headers.set(::hyper::header::Server("h2o/1.7.0".into()));

    BufferedResponse {
        body: "Hello, world!".as_bytes().to_vec(),
        response: hydra::Response {
            status: StatusCode::Ok,
            headers: headers,
        },
    }
}

#[test]
fn test_response_spec_macro() {
    response_spec! {
        type_name => SimpleGet,
        status => StatusCode::Ok,
        body_contains => ["Hello", "world"],
        headers => {
            "Server" => "h2o/1.7.0"
        }
    }

    SimpleGet::check(&response_spec_test_response());
}

#[test]
#[should_panic]
fn test_response_spec_macro_body_doesnt_contain() {
    response_spec! {
        type_name => SimpleGet,
        status => StatusCode::Ok,
        body_contains => ["Goodbye", "cruel", "world"],
        headers => {
            "Server" => "h2o/1.7.0"
        }
    }

    SimpleGet::check(&response_spec_test_response());
}

#[test]
#[should_panic]
fn test_response_spec_macro_different_status() {
    response_spec! {
        type_name => SimpleGet,
        status => StatusCode::NotFound,
        body_contains => [],
        headers => { }
    }

    SimpleGet::check(&response_spec_test_response());
}

#[test]
#[should_panic]
fn test_response_spec_macro_missing_header() {
    response_spec! {
        type_name => SimpleGet,
        status => StatusCode::Ok,
        body_contains => [],
        headers => {
            "NotAHeader" => "Nope"
        }
    }

    SimpleGet::check(&response_spec_test_response());
}

#[test]
#[should_panic]
fn test_response_spec_macro_header_wrong_value() {
    response_spec! {
        type_name => SimpleGet,
        status => StatusCode::Ok,
        body_contains => [],
        headers => {
            "Server" => "Nope"
        }
    }

    SimpleGet::check(&response_spec_test_response());
}
