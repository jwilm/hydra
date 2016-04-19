extern crate hydra;
extern crate hyper;
extern crate env_logger;

use std::sync::mpsc;

use hydra::prelude::*;

#[macro_use]
mod util;

use util::*;

response_spec! {
    type_name => SimpleGetChecker,
    status => StatusCode::Ok,
    body_contains => [],
    headers => {
        "Server" => "h2o"
    }
}

/// Test that several GET requests can run in parallel on a single thread.
#[test]
fn three_get_streams_one_worker() {
    util::enable_logging();

    let mut config = hydra::Config::default();
    config.threads = 1;

    let (tx, rx) = mpsc::channel();
    let handler = ConnectHandler::new(tx);

    let cluster = Hydra::new(&config).unwrap();
    let mut collector = ResponseCollector::<HeadersOnlyHandler>::new();

    cluster.connect("http2bin.org:80", handler).unwrap();

    // Wait for the connection.
    let client = match rx.recv().unwrap() {
        ConnectionMsg::Connected(conn) => conn,
        _ => panic!("Didn't recv Connected"),
    };

    for _ in 0..3 {
        let req = Request::new_headers_only(Method::Get, "/get", Headers::new());
        client.request(req, collector.new_stream_handler()).unwrap();
    }

    collector.wait_all();
    collector.check_responses(SimpleGetChecker);
}

/// Test that several GET request can run in parallel on separate threads
#[test]
fn three_workers_three_get_each() {
    util::enable_logging();

    let mut config = hydra::Config::default();
    config.threads = 3;

    let (tx, rx) = mpsc::channel();
    let handler1 = ConnectHandler::new(tx.clone());
    let handler2 = ConnectHandler::new(tx.clone());
    let handler3 = ConnectHandler::new(tx.clone());

    let cluster = Hydra::new(&config).unwrap();
    let mut collector = ResponseCollector::<HeadersOnlyHandler>::new();

    cluster.connect("http2bin.org:80", handler1).unwrap();
    cluster.connect("http2bin.org:80", handler2).unwrap();
    cluster.connect("http2bin.org:80", handler3).unwrap();

    // Wait for the connection.
    match rx.recv().unwrap() {
        ConnectionMsg::Connected(conn) => {
            for _ in 0..3 {
                let req = Request::new_headers_only(Method::Get, "/get", Headers::new());
                conn.request(req, collector.new_stream_handler()).unwrap();
            }
        },
        _ => panic!("Didn't recv Connected"),
    };

    collector.wait_all();
    collector.check_responses(SimpleGetChecker);
}

#[test]
fn make_some_post_requests() {
    util::enable_logging();

    response_spec! {
        type_name => PostChecker,
        status => StatusCode::Ok,
        body_contains => ["Hello, world!"],
        headers => { }
    }

    let mut config = hydra::Config::default();
    config.threads = 1;

    let (tx, rx) = mpsc::channel();
    let handler = ConnectHandler::new(tx);

    let cluster = Hydra::new(&config).unwrap();
    let mut collector = ResponseCollector::<BodyWriter<HelloWorld>>::new();

    cluster.connect("http2bin.org:80", handler).unwrap();

    // Wait for the connection.
    let client = match rx.recv().unwrap() {
        ConnectionMsg::Connected(conn) => conn,
        _ => panic!("Didn't recv Connected"),
    };

    for _ in 0..3 {
        let req = Request::new(Method::Post, "/post", Headers::new());
        client.request(req, collector.new_stream_handler()).unwrap();
    }

    collector.wait_all();
    collector.check_responses(PostChecker);
}

#[test]
fn concurrent_requests_greater_than_inflight_limit() {
    util::enable_logging();

    let mut config = hydra::Config::default();
    config.threads = 1;

    let (tx, rx) = mpsc::channel();
    let handler = ConnectHandler::new(tx);

    let cluster = Hydra::new(&config).unwrap();
    let mut collector = ResponseCollector::<HeadersOnlyHandler>::new();

    cluster.connect("http2bin.org:80", handler).unwrap();

    // Wait for the connection.
    let client = match rx.recv().unwrap() {
        ConnectionMsg::Connected(conn) => conn,
        _ => panic!("Didn't recv Connected"),
    };

    // http2bin maxes at 100 in flight
    for _ in 0..150 {
        let req = Request::new_headers_only(Method::Get, "/get", Headers::new());
        client.request(req, collector.new_stream_handler()).unwrap();
    }

    collector.wait_all();

    // just checking here that responses are received for all of them. They should probably all be
    // successful, though.
}

#[test]
fn error_during_data_stream() {
    util::enable_logging();

    let mut config = hydra::Config::default();
    config.threads = 1;

    let (tx, rx) = mpsc::channel();
    let handler = ConnectHandler::new(tx);

    let cluster = Hydra::new(&config).unwrap();
    let mut collector = ResponseCollector::<HandlerStreamError>::new();

    cluster.connect("http2bin.org:80", handler).unwrap();

    // Wait for the connection.
    let client = match rx.recv().unwrap() {
        ConnectionMsg::Connected(conn) => conn,
        _ => panic!("Didn't recv Connected"),
    };

    let req = Request::new(Method::Post, "/post", Headers::new());
    client.request(req, collector.new_stream_handler()).unwrap();

    collector.wait_all();
    let messages = collector.messages();
    assert_eq!(messages.len(), 1);
    for message in messages {
        match message {
            &StreamMsg::Error(ref err) => {
                match *err {
                    request::Error::User => (),
                    _ => panic!("unexpected stream error: {:?}", err),
                }
            },
            _ => panic!("did not expect valid response: {:?}", message),
        }
    }
}
