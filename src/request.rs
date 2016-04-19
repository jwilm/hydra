use std::fmt;

use header::Headers;

use protocol;

use ::Method;
use ::Http2ErrorCode;

/// Stream events and Request/Response bytes are delivered to/from the handler.
///
/// Functions of the Handler are called on the event loop's thread. Any work done here should
/// be as short as possible to not delay network I/O and processing of other streams.
pub trait Handler: Send + fmt::Debug + 'static {
    /// Provide data from the request body
    ///
    /// This will be called repeatedly until one of StreamDataState::{Last, Error} are returned
    fn stream_data(&mut self, &mut [u8]) -> protocol::StreamDataState;

    /// Response headers are available
    fn on_response(&mut self, res: Response);

    /// Data from the response is available
    fn on_response_data(&mut self, data: &[u8]);

    /// Called when the stream is closed (complete)
    fn on_close(&mut self);

    /// Error occurred
    fn on_error(&mut self, err: Error);
}

/// Response for a given request
#[derive(Debug)]
pub struct Response {
    pub status: ::hyper::status::StatusCode,
    pub headers: Headers,
}

/// Errors that may occur during an HTTP/2 request
#[derive(Debug)]
pub enum Error {
    /// The remote end terminated this stream. No more data will arrive.
    Reset(Http2ErrorCode),

    /// request cannot be completed because the connection entered an error state
    Connection,

    /// Request cannot be completed because an error was returned from a stream handler method
    User,

    /// Received a GOAWAY frame on the connection where this request was being processed. This
    /// stream was not handled by the server.
    GoAwayUnprocessed(Http2ErrorCode),

    /// Received a GOAWAY frame on the connection handling this request; this stream was potentially
    /// processed by the server.
    GoAwayMaybeProcessed(Http2ErrorCode),
}

impl ::std::error::Error for Error {
    fn cause(&self) -> Option<&::std::error::Error> {
        None
    }

    fn description(&self) -> &str {
        match *self {
            Error::Reset(_) => "stream reset by peer",
            Error::Connection => "connection encountered error",
            Error::User => "error during stream handler call",
            Error::GoAwayUnprocessed(_) => "GOAWAY frame received; stream not processed",
            Error::GoAwayMaybeProcessed(_) => {
                "GOAWAY frame received; stream maybe processed"
            },
        }
    }
}

impl ::std::fmt::Display for Error {
    fn fmt(&self, f: &mut ::std::fmt::Formatter) -> ::std::fmt::Result {
        try!(write!(f, "Request failed because "));
        match *self {
            Error::Reset(code) => write!(f, "stream was reset by peer: {:?}", code),
            Error::Connection => write!(f, "connection encountered an error"),
            Error::User => write!(f, "an error was returned from a stream handler call"),
            Error::GoAwayUnprocessed(code) => {
                write!(f, "Received a GOAWAY, stream unprocessed; code: {:?}", code)
            },
            Error::GoAwayMaybeProcessed(code) => {
                write!(f, "Received a GOAWAY, stream maybe processed; code: {:?}", code)
            },
        }
    }
}

#[derive(Debug)]
pub struct Request {
    pub method: Method,
    pub headers: Headers,
    pub path: String,
    pub headers_only: bool,
}

impl Request {
    pub fn new<P>(method: Method, path: P, headers: Headers) -> Request
        where P: Into<String>,
    {
        Request {
            method: method,
            path: path.into(),
            headers_only: false,
            headers: headers,
        }
    }

    pub fn new_headers_only<P>(method: Method, path: P, headers: Headers) -> Request
        where P: Into<String>,
    {
        Request {
            method: method,
            path: path.into(),
            headers_only: true,
            headers: headers,
        }
    }
}
