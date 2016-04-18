//! Header types and utilities
use std::str;
use std::ascii::AsciiExt;

use httparse;

use hyper::header::Headers;
use hyper::status::StatusCode;

use solicit::http::Header;

/// Error resulting from parsing solicit headers
#[derive(Debug)]
pub enum ParseError {
    Utf8(::std::str::Utf8Error),
    HyperFromRaw(::hyper::Error),
    StatusNotNumber(::std::num::ParseIntError),
    StatusMissing,
}

impl ::std::error::Error for ParseError {
    fn cause(&self) -> Option<&::std::error::Error> {
        match *self {
            ParseError::Utf8(ref err) => Some(err),
            ParseError::HyperFromRaw(ref err) => Some(err),
            ParseError::StatusNotNumber(ref err) => Some(err),
            _ => None,
        }
    }

    fn description(&self) -> &str {
        match *self {
            ParseError::Utf8(ref err) => err.description(),
            ParseError::HyperFromRaw(ref err) => err.description(),
            ParseError::StatusNotNumber(_) => ":status pseudo header not a number",
            ParseError::StatusMissing => ":status pseudo header missing",
        }
    }
}

impl ::std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut ::std::fmt::Formatter) -> ::std::fmt::Result {
        match *self {
            ParseError::Utf8(ref err) => {
                write!(f, "Failed parsing header name as utf8: {}", err)
            },
            ParseError::HyperFromRaw(ref err) => {
                write!(f, "Failed to build hyper headers from raw headers: {}", err)
            },
            ParseError::StatusNotNumber(ref err) => {
                write!(f, "Failed to parse :status pseudo header as integer: {}", err)
            },
            ParseError::StatusMissing => write!(f, ":status pseudo header was not included"),
        }
    }
}

impl From<::std::str::Utf8Error> for ParseError {
    fn from(val: ::std::str::Utf8Error) -> ParseError {
        ParseError::Utf8(val)
    }
}

impl From<::hyper::Error> for ParseError {
    fn from(val: ::hyper::Error) -> ParseError {
        ParseError::HyperFromRaw(val)
    }
}

impl From<::std::num::ParseIntError> for ParseError {
    fn from(val: ::std::num::ParseIntError) -> ParseError {
        ParseError::StatusNotNumber(val)
    }
}

pub type ParseResult<T> = ::std::result::Result<T, ParseError>;

pub fn to_status_and_headers<'n, 'v>(headers: Vec<Header<'n, 'v>>)
    -> ParseResult<(StatusCode, Headers)>
{
    let headers = try!(to_hyper(headers));

    let status_code = match headers.get_raw(":status") {
        Some(ref raw_statuses) => {
            // raw_statuses is a &[Vec<u8>]
            assert!(raw_statuses.len() > 0);
            let raw_status_bytes = &raw_statuses[0];
            let raw_status_str = try!(::std::str::from_utf8(&raw_status_bytes[..]));
            let raw_status_num = try!(raw_status_str.parse::<u16>());
            StatusCode::from_u16(raw_status_num)
        },
        _ => return Err(ParseError::StatusMissing),
    };

    Ok((status_code, headers))
}

/// Parses raw solicit headers into hyper headers
pub fn to_hyper(headers: Vec<Header>) -> ParseResult<Headers> {
    let httparse_headers = try!(headers.iter().map(|header| {
        Ok(httparse::Header {
            name: try!(str::from_utf8(&header.name())),
            value: &header.value(),
        })
    }).collect::<Result<Vec<_>, ParseError>>());

    // TODO from_raw does a copy of the httparse::Header values. We can provide owned values,
    // so the copies are wasteful.
    Ok(try!(Headers::from_raw(&httparse_headers[..])))
}

// A helper function that prepares the headers that should be sent in an HTTP/2 message.
//
// Adapts hyper `Headers` into a list of solicit's `Header` type.
pub fn to_h2(mut headers: Headers) -> Vec<Header<'static, 'static>> {
    if headers.remove::<::hyper::header::Connection>() {
        warn!("The `Connection` header is not valid for an HTTP/2 connection.");
    }

    let mut http2_headers: Vec<_> = headers.iter().filter_map(|h| {
        if h.is::<::hyper::header::SetCookie>() {
            None
        } else {
            // HTTP/2 header names MUST be lowercase.
            Some((h.name().to_ascii_lowercase().into_bytes(), h.value_string().into_bytes()))
        }
    }).collect();

    // Now separately add the cookies, as `hyper` considers `Set-Cookie` to be only a single
    // header, even in the face of multiple cookies being set.
    if let Some(set_cookie) = headers.get::<::hyper::header::SetCookie>() {
        for cookie in set_cookie.iter() {
            http2_headers.push((b"set-cookie".to_vec(), cookie.to_string().into_bytes()));
        }
    }

    http2_headers
        .into_iter()
        .map(|(name, value)| Header::new(name, value))
        .collect()
}
