extern crate hydra;
extern crate env_logger;
extern crate hyper;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;

use hydra::{Method, Request, Hydra};
use hydra::{connection, protocol};
use hydra::Headers;

mod util;

use util::*;

/// Test that several GET requests can run in parallel on a single thread.
#[test]
fn three_get_streams_one_worker() {
    env_logger::init().ok();

    let mut config = hydra::Config::default();
    config.threads = 1;

    let (tx, rx) = mpsc::channel();
    let handler = ConnectHandler::new(tx);

    let cluster = Hydra::new(&config);
    let collector = ResponseCollector::new();

    cluster.connect("http2bin.org:80", handler).unwrap();

    // Wait for the connection.
    let client = match rx.recv().unwrap() {
        ConnectionMsg::Connected(conn) => conn,
        _ => panic!("Didn't recv Connected"),
    };

    for _ in 0..3 {
        let req = Request::new_headers_only(Method::Get, "/get", Headers::new());
        client.request(req, collector.new_stream_handler());
    }

    collector.wait_all();
}

/// Test that several GET request can run in parallel on separate threads
#[test]
fn three_workers_three_get_each() {
    env_logger::init().ok();

    let mut config = hydra::Config::default();
    config.threads = 3;

    let (tx, rx) = mpsc::channel();
    let handler1 = ConnectHandler::new(tx.clone());
    let handler2 = ConnectHandler::new(tx.clone());
    let handler3 = ConnectHandler::new(tx.clone());

    let cluster = Hydra::new(&config);
    let collector = ResponseCollector::new();

    cluster.connect("http2bin.org:80", handler1).unwrap();
    cluster.connect("http2bin.org:80", handler2).unwrap();
    cluster.connect("http2bin.org:80", handler3).unwrap();

    // Wait for the connection.
    match rx.recv().unwrap() {
        ConnectionMsg::Connected(conn) => {
            for _ in 0..3 {
                let req = Request::new_headers_only(Method::Get, "/get", Headers::new());
                conn.request(req, collector.new_stream_handler());
            }
        },
        _ => panic!("Didn't recv Connected"),
    };

    collector.wait_all();
}
