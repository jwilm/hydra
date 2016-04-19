//! Helpers for writing Hydra tests
use std::fmt;
use std::io::Cursor;
use std::marker::PhantomData;
use std::sync::mpsc;

use hydra::prelude::*;

/// Messages sent by ConnectHandler
pub enum ConnectionMsg {
    Error(connection::Error),
    Connected(connection::Handle),
    Pong
}

/// Messages sent by a StreamHandler
#[derive(Debug)]
pub enum StreamMsg {
    Error(request::Error),
    Response(BufferedResponse),
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

    fn on_error(&self, err: connection::Error) {
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
    rx: mpsc::Receiver<StreamMsg>,
    tx: mpsc::Sender<StreamMsg>,
    counter: usize,
    messages: Vec<StreamMsg>,
    _marker: PhantomData<S>
}

pub struct BufferedResponse {
    pub response: Response,
    pub body: Vec<u8>,
}

impl fmt::Debug for BufferedResponse {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("BufferedResponse")
            .field("response", &self.response)
            .field("body", &::std::str::from_utf8(&self.body[..]))
            .finish()
    }
}

impl fmt::Display for BufferedResponse {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Response\n\
                   ========\n\
                   {}\n\
                   {:?}\n", self.response.headers, ::std::str::from_utf8(&self.body[..]))
    }
}

impl<S> Default for ResponseCollector<S> {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        ResponseCollector {
            rx: rx,
            tx: tx,
            counter: 0,
            messages: Vec::new(),
            _marker: PhantomData
        }
    }
}

impl<S> ResponseCollector<S> {
    pub fn messages(&self) -> &[StreamMsg] {
        &self.messages[..]
    }
}

impl<S: Collectable> ResponseCollector<S> {
    /// Builds a ResponseCollector
    pub fn new() -> ResponseCollector<S> {
        Default::default()
    }

    /// Get a stream handler
    ///
    /// Returns a StreamHandler and increments the number of messages `wait_all` expects
    pub fn new_stream_handler(&mut self) -> S {
        self.counter += 1;

        S::new(self.tx.clone())
    }

    /// Block until a response is received for every stream handler.
    ///
    /// TODO would be nice if this could time out; it currently blocks indefinitely.
    pub fn wait_all(&mut self) {
        while self.counter != 0 {
            let msg = self.rx.recv().unwrap();
            self.counter -= 1;
            self.messages.push(msg);
            println!("waiting for {} streams", self.counter);
        }
    }

    /// Run a checker against the buffered messages.
    pub fn check_responses<C>(&self, _: C)
        where C: CheckResponse,
    {
        for msg in &self.messages {
            match *msg {
                StreamMsg::Error(ref err) => panic!("got error response: {}", err),
                StreamMsg::Response(ref res) => C::check(res)
            }
        }
    }
}

pub trait Collectable: request::Handler {
    fn new(tx: mpsc::Sender<StreamMsg>) -> Self;
}

/// Implementor of request::Handler returns an error in `stream_data`.
#[derive(Debug)]
pub struct HandlerStreamError {
    tx: mpsc::Sender<StreamMsg>,
    body: Vec<u8>,
    response: Option<Response>,
    generated_error: bool,
}

impl Collectable for HandlerStreamError {
    fn new(tx: mpsc::Sender<StreamMsg>) -> HandlerStreamError {
        HandlerStreamError {
            tx: tx,
            body: Vec::new(),
            response: None,
            generated_error: false,
        }
    }
}

impl request::Handler for HandlerStreamError {
    fn on_error(&mut self, err: request::Error) {
        self.tx.send(StreamMsg::Error(err)).unwrap();
    }

    fn on_response_data(&mut self, _bytes: &[u8]) {
        unreachable!();
    }

    fn stream_data(&mut self, _buf: &mut [u8]) -> StreamDataState {
        assert!(!self.generated_error);
        self.generated_error = true;
        StreamDataState::error()
    }

    /// Response headers are available
    fn on_response(&mut self, _res: Response) {
        unreachable!();
    }

    /// Called when the stream is closed (complete)
    fn on_close(&mut self) {
        unreachable!();
    }
}

/// Implementor of request::Handler receives stream events
///
/// When an error occurs or the stream terminates normally, an event is sent on the channel passed
/// to `new`.
#[derive(Debug)]
pub struct HeadersOnlyHandler {
    tx: mpsc::Sender<StreamMsg>,
    body: Vec<u8>,
    response: Option<Response>,
}

impl Collectable for HeadersOnlyHandler {
    fn new(tx: mpsc::Sender<StreamMsg>) -> HeadersOnlyHandler {
        HeadersOnlyHandler {
            tx: tx,
            body: Vec::new(),
            response: None,
        }
    }
}

impl request::Handler for HeadersOnlyHandler {
    fn on_error(&mut self, err: request::Error) {
        self.tx.send(StreamMsg::Error(err)).unwrap();
    }

    fn on_response_data(&mut self, bytes: &[u8]) {
        self.body.extend_from_slice(bytes);
    }

    fn stream_data(&mut self, _buf: &mut [u8]) -> StreamDataState {
        unimplemented!();
    }

    /// Response headers are available
    fn on_response(&mut self, res: Response) {
        self.response = Some(res);
    }

    /// Called when the stream is closed (complete)
    fn on_close(&mut self) {
        let mut res = Vec::new();
        ::std::mem::swap(&mut self.body, &mut res);

        let response = self.response.take().unwrap();

        self.tx.send(StreamMsg::Response(BufferedResponse {
            response: response,
            body: res,
        })).unwrap();
    }
}

pub trait GetBody : fmt::Debug + Send + 'static {
    fn get_body() -> Vec<u8>;
}

#[derive(Debug)]
pub struct HelloWorld;

impl GetBody for HelloWorld {
    fn get_body() -> Vec<u8> {
        "Hello, world!".as_bytes().to_vec()
    }
}

#[derive(Debug)]
pub struct BodyWriter<G: GetBody> {
    tx: mpsc::Sender<StreamMsg>,
    outgoing: Cursor<Vec<u8>>,
    body: Vec<u8>,
    response: Option<Response>,
    _marker: PhantomData<G>,
}

impl<G> Collectable for BodyWriter<G>
    where G: GetBody,
{
    fn new(tx: mpsc::Sender<StreamMsg>) -> BodyWriter<G> {
        BodyWriter {
            tx: tx,
            body: Vec::new(),
            response: None,
            outgoing: Cursor::new(G::get_body()),
            _marker: PhantomData,
        }
    }
}

impl<G> request::Handler for BodyWriter<G>
    where G: GetBody,
{
    fn on_error(&mut self, err: request::Error) {
        self.tx.send(StreamMsg::Error(err)).unwrap();
    }

    fn on_response_data(&mut self, bytes: &[u8]) {
        self.body.extend_from_slice(bytes);
    }

    fn stream_data(&mut self, buf: &mut [u8]) -> StreamDataState {
        use std::io::Read;

        let read = match self.outgoing.read(buf) {
            Ok(count) => count,
            Err(_) => return StreamDataState::error(),
        };
        let pos = self.outgoing.position() as usize;
        let total = self.outgoing.get_ref().len();

        if total == pos {
            StreamDataState::done(read)
        } else {
            StreamDataState::read(read)
        }
    }

    /// Response headers are available
    fn on_response(&mut self, res: Response) {
        self.response = Some(res);
    }

    /// Called when the stream is closed (complete)
    fn on_close(&mut self) {
        let mut res = Vec::new();
        ::std::mem::swap(&mut self.body, &mut res);

        let response = self.response.take().unwrap();

        self.tx.send(StreamMsg::Response(BufferedResponse {
            response: response,
            body: res,
        })).unwrap();
    }
}

pub fn enable_logging() {
    ::env_logger::init().ok();
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

        #[allow(unused_variables)]
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
        response: Response {
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
