//! Header types and utilities

#[derive(Debug)]
pub enum ConversionError {
    Utf8(::std::str::Utf8Error),
    HyperFromRaw(::hyper::Error),
}

impl ::std::error::Error for ConversionError {
    fn cause(&self) -> Option<&::std::error::Error> {
        match *self {
            ConversionError::Utf8(ref err) => Some(err),
            ConversionError::HyperFromRaw(ref err) => Some(err),
        }
    }

    fn description(&self) -> &str {
        match *self {
            ConversionError::Utf8(ref err) => err.description(),
            ConversionError::HyperFromRaw(ref err) => err.description(),
        }
    }
}

impl ::std::fmt::Display for ConversionError {
    fn fmt(&self, f: &mut ::std::fmt::Formatter) -> ::std::fmt::Result {
        match *self {
            ConversionError::Utf8(ref err) => {
                write!(f, "Failed parsing header name as utf8: {}", err)
            },
            ConversionError::HyperFromRaw(ref err) => {
                write!(f, "Failed to build hyper headers from raw headers: {}", err)
            },
        }
    }
}

impl From<::std::str::Utf8Error> for ConversionError {
    fn from(val: ::std::str::Utf8Error) -> ConversionError {
        ConversionError::Utf8(val)
    }
}

impl From<::hyper::Error> for ConversionError {
    fn from(val: ::hyper::Error) -> ConversionError {
        ConversionError::HyperFromRaw(val)
    }
}

use std::str;
use std::ascii::AsciiExt;

use httparse;

use solicit::http::Header;

use hyper::header::Headers;

/// Parses raw solicit headers into hyper headers
pub fn to_hyper(headers: Vec<Header>) -> Result<Headers, ConversionError> {
    let httparse_headers = try!(headers.iter().map(|header| {
        Ok(httparse::Header {
            name: try!(str::from_utf8(&header.name())),
            value: &header.value(),
        })
    }).collect::<Result<Vec<_>, ConversionError>>());

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
