use std::fmt;
use std::collections::HashSet;
use std::io::Cursor;

use solicit::http::frame::{self, RawFrame, FrameIR, HttpSetting};
use solicit::http::{HttpScheme, HttpResult, Header, StreamId, ErrorCode};
use solicit::http::priority::DataPrioritizer;
use solicit::http::connection::{HttpConnection, SendFrame, ReceiveFrame, HttpFrame, EndStream};
use solicit::http::connection::SendStatus;
use solicit::http::connection::DataChunk;
use solicit::http::HttpError;
use solicit::http::client::{write_preface, RequestStream};
use solicit::http::session::{DefaultSessionState, SessionState, StreamState};
use solicit::http::session::{self, Stream as SessionStream};

use super::RequestError;

use hyper::header::Headers;

use connection::{self, DispatchConnectionEvent};

use header;

pub use solicit::http::session::{StreamDataChunk, StreamDataError};


/// Upon successfully reading from a stream, ReadInfo is provided in the StreamDataState
#[derive(Debug, Eq, PartialEq)]
pub enum ReadInfo {
    Pending(usize),
    End(usize),
}

/// Result of reading stream outgoing data
#[derive(Eq, PartialEq, Debug)]
pub enum StreamDataState {
    /// Have bytes to send
    Read(ReadInfo),

    /// Will check back later for data
    Unavailable,

    /// Cannot continue due to error
    ///
    /// Returning this will terminate the stream immediately.
    Error,
}

impl StreamDataState {
    pub fn done(bytes: usize) -> StreamDataState {
        StreamDataState::Read(ReadInfo::End(bytes))
    }

    pub fn read(bytes: usize) -> StreamDataState {
        StreamDataState::Read(ReadInfo::Pending(bytes))
    }

    pub fn unavailable() -> StreamDataState {
        StreamDataState::Unavailable
    }

    pub fn error() -> StreamDataState {
        StreamDataState::Error
    }
}

/// Type returned from stream_data
///
/// Contains a stream_id and its state.
struct StreamResult {
    id: StreamId,
    state: StreamDataState,
}

#[derive(Debug)]
pub struct Response {
    pub status: ::hyper::status::StatusCode,
    pub headers: Headers,
}

/// Prioritizer where the buffer is already provided and filled; it just needs to be returned from
/// the data chunk.
pub struct BufferPrioritizer<'a> {
    /// Session state where streams are stored
    buf: &'a mut [u8],
    end: EndStream,
    id: StreamId,
}

impl<'a> BufferPrioritizer<'a> {
    /// Create a BufferPrioritizer.
    ///
    /// Streams will be selected using the interest registry, and stream data will be read from the
    /// provided State object.
    pub fn new(id: StreamId, buf: &'a mut [u8], end: EndStream) -> BufferPrioritizer<'a>
    {
        BufferPrioritizer {
            buf: buf,
            end: end,
            id: id,
        }
    }
}

impl<'a> DataPrioritizer for BufferPrioritizer<'a> {
    /// Provide the wrapped buffer
    fn get_next_chunk(&mut self) -> HttpResult<Option<DataChunk>> {
        Ok(Some(DataChunk::new_borrowed(&self.buf[..], self.id, self.end)))
    }
}

/// Stream events and Request/Response bytes are delivered to/from the handler.
///
/// Functions of the StreamHandler are called on the event loop's thread. Any work done here should
/// be as short as possible to not delay network I/O and processing of other streams.
pub trait StreamHandler: Send + fmt::Debug + 'static {
    /// Provide data from the request body
    ///
    /// This will be called repeatedly until one of StreamDataState::{Last, Error} are returned
    fn stream_data(&mut self, &mut [u8]) -> StreamDataState;

    /// Response headers are available
    fn on_response(&mut self, res: Response);

    /// Data from the response is available
    fn on_response_data(&mut self, data: &[u8]);

    /// Called when the stream is closed (complete)
    fn on_close(&mut self);

    /// Error occurred
    fn on_error(&mut self, err: super::RequestError);
}

pub trait Protocol: Sized + 'static {
    fn new() -> Self;
    fn on_data(&mut self, buf: &[u8], conn: connection::Ref) -> HttpResult<usize>;
    fn ready_write(&mut self, conn: connection::Ref) -> HttpResult<SendStatus>;
    fn notify(&mut self, msg: Msg, conn: connection::Ref) -> HttpResult<()>;
}

/// Messages for an HTTP/2 state machine
#[derive(Debug)]
pub enum Msg {
    /// Create a new stream on the current connection
    CreateStream(::Request, Box<StreamHandler>),

    /// Send a PING to the other end of connection
    Ping,
}

/// SendFrame implementor that pushes onto the connection outgoing frame list.
struct SendDirect<'brw, 'conn> where 'conn: 'brw {
    conn: &'brw mut connection::Ref<'conn>,
}

impl<'a, 'b> SendFrame for SendDirect<'a, 'b> {
    fn send_frame<F: FrameIR>(&mut self, frame: F) -> HttpResult<()> {
        trace!("send_frame");
        let mut buf = Cursor::new(Vec::with_capacity(1024));
        try!(frame.serialize_into(&mut buf));
        self.conn.queue_frame(buf.into_inner());
        Ok(())
    }
}

/// Receiver that will yield precisely one frame
///
/// If a WrapperReceive can be successfully constructed from `parse`, `recv_frame` may be used to
/// consume the RawFrame. Since HttpFrame::from_raw requires an owned RawFrame, it is held in an
/// option to be moved at some point.
struct WrappedReceive<'a> {
    frame: RawFrame<'a>,
}

impl<'a> WrappedReceive<'a> {
    /// Attempts to read an entire frame from the transport read buffer.
    ///
    /// In the case where not enough bytes are available to construct a RawFrame, parse returns
    /// None.
    fn parse(buf: &'a [u8]) -> Option<WrappedReceive<'a>> {
        RawFrame::parse(buf).map(|frame| WrappedReceive {
            frame: frame,
        })
    }

    pub fn frame(&self) -> &RawFrame<'a> {
        &self.frame
    }
}

impl<'a> ReceiveFrame for WrappedReceive<'a> {
    /// Take the wrapped frame and parse it
    fn recv_frame(&mut self) -> HttpResult<HttpFrame> {
        HttpFrame::from_raw(&self.frame)
    }
}

pub struct Stream {
    state: session::StreamState,
    inner: Box<StreamHandler>,
}

impl Stream {
    #[inline]
    pub fn new(handler: Box<StreamHandler>) -> Stream {
        Stream {
            // TODO should really start in Idle state
            state: session::StreamState::Open,
            inner: handler,
        }
    }

    #[inline]
    pub fn on_response(&mut self, res: Response) {
        self.inner.on_response(res);
    }

    pub fn stream_data(&mut self, buf: &mut [u8]) -> StreamDataState {
        self.inner.stream_data(buf)
    }

    fn on_error(&mut self, err: super::RequestError) {
        self.inner.on_error(err)
    }
}

impl session::Stream for Stream {
    fn new_data_chunk(&mut self, data: &[u8]) {
        self.inner.on_response_data(data)
    }

    fn set_headers(&mut self, _headers: Vec<Header>) {
        // This method is unused since the type signature does not support parsing headers as the
        // hyper headers type (needs ability to return an error).
        unimplemented!();
    }

    fn set_state(&mut self, state: StreamState) {
        trace!("protocol::Stream::set_state {:?}", state);
        self.state = state;
        if state == StreamState::Closed {
            self.inner.on_close();
        }
    }

    fn get_data_chunk(&mut self, _buf: &mut [u8]) -> Result<StreamDataChunk, StreamDataError> {
        unreachable!();
    }

    /// Returns the current state of the stream.
    fn state(&self) -> StreamState {
        self.state
    }
}

pub struct Http2 {
    got_settings: bool,
    conn: HttpConnection,
    state: DefaultSessionState<session::Client, Stream>,

    /// Dynamic protocol configuration
    settings: Http2Settings,

    /// Streams with active interest in writing data
    interest: HashSet<StreamId>,
}

/// Settings type that exposes some properties via Send/Sync types
pub struct Http2Settings {
    /// Max concurrent streams for local use
    max_concurrent_streams: u32
}

impl Default for Http2Settings {
    fn default() -> Self {
        Http2Settings {
            max_concurrent_streams: 10,
        }
    }
}

impl Settings for Http2Settings {
    fn set_max_concurrent_streams(&mut self, val: u32) {
        self.max_concurrent_streams = val;
    }
}

/// Defines interactions with an object holding connection settings.
///
/// Methods on the Settings type are used only when a SETTINGS frame is received for the associated
/// connection.
pub trait Settings {
    /// Set the maximum number of concurrent streams
    fn set_max_concurrent_streams(&mut self, val: u32);
}

impl Http2 {
    /// Create a RequestStream given a request and handler
    ///
    /// This is a convenience method for generating a RequestStream. It is not added to the
    /// protocol's active streams; that is up to the caller.
    fn new_stream(request: ::Request,
                  handler: Box<StreamHandler>,
                  scheme: HttpScheme) -> RequestStream<Stream>
    {
        let ::Request { method, path, headers_only, mut headers } = request;

        trace!("new_stream: scheme={:?}, method={}", scheme, method);

        headers.set(::hyper::header::UserAgent("hydra/0.0.1".into()));

        let mut solicit_headers: Vec<Header> = vec![
            Header::new(b":method", format!("{}", method).into_bytes()),
            Header::new(b":path", path.into_bytes()),
            Header::new(b":authority", &b"http2bin.org"[..]),
            Header::new(b":scheme", scheme.as_bytes().to_vec()),
        ];

        solicit_headers.extend_from_slice(&header::to_h2(headers)[..]);

        let mut stream = Stream::new(handler);
        if headers_only {
            stream.set_state(StreamState::HalfClosedLocal);
        }

        RequestStream {
            headers: solicit_headers,
            stream: stream,
        }
    }

    /// Returns the scheme of the underlying `HttpConnection`.
    #[inline]
    pub fn scheme(&self) -> HttpScheme {
        self.conn.scheme
    }

    /// Handles the next frame provided by the given frame receiver and expects it to be a
    /// `SETTINGS` frame. If it is not, it returns an error.
    ///
    /// The method is a convenience method that can be used during the initialization of the
    /// connection, as the first frame that any peer is allowed to send is an initial settings
    /// frame.
    pub fn expect_settings<Recv: ReceiveFrame, Sender: SendFrame>(&mut self,
                                                                  rx: &mut Recv,
                                                                  tx: &mut Sender)
                                                                  -> HttpResult<()> {
        let mut session = ClientSession::new(&mut self.state, tx, &mut self.settings);
        self.conn.expect_settings(rx, &mut session)
    }

    /// Starts a new request based on the given `RequestStream`.
    ///
    /// For now it does not perform any validation whether the given `RequestStream` is valid.
    pub fn start_request<S: SendFrame>(&mut self,
                                       req: RequestStream<Stream>,
                                       sender: &mut S)
                                       -> HttpResult<StreamId> {

        let end_stream = if req.stream.is_closed_local() {
            EndStream::Yes
        } else {
            EndStream::No
        };

        let stream_id = self.state.insert_outgoing(req.stream);

        if end_stream == EndStream::No {
            // mark stream as interested in writing
            self.interest.insert(stream_id);
        }

        try!(self.conn.sender(sender).send_headers(req.headers, stream_id, end_stream));

        debug!("CreatedStream {:?}", stream_id);
        Ok(stream_id)
    }

    /// Send a PING
    pub fn send_ping<S: SendFrame>(&mut self, sender: &mut S) -> HttpResult<()> {
        self.conn.sender(sender).send_ping(0)
    }

    /// Fully handles the next incoming frame provided by the given `ReceiveFrame` instance.
    /// Handling a frame may cause changes to the session state exposed by the `ClientConnection`.
    pub fn handle_next_frame<Recv: ReceiveFrame, Sender: SendFrame>(&mut self,
                                                                    rx: &mut Recv,
                                                                    tx: &mut Sender)
                                                                    -> HttpResult<()> {
        trace!("handle_next_frame");
        // TODO apparently handle_next_frame will return an error when GOAWAY is received
        let mut session = ClientSession::new(&mut self.state, tx, &mut self.settings);
        self.conn.handle_next_frame(rx, &mut session)
    }

    /// Queues a new DATA frame onto the underlying `SendFrame`.
    ///
    /// Currently, no prioritization of streams is taken into account and which stream's data is
    /// queued cannot be relied on.
    pub fn send_next_data<S: SendFrame>(&mut self, sender: &mut S) -> HttpResult<SendStatus> {
        trace!("Http2::send_next_data");
        // A default "maximum" chunk size of 8 KiB is set on all data frames.
        const MAX_CHUNK_SIZE: usize = 8_192;
        let mut buf = [0; MAX_CHUNK_SIZE];

        if let Some(stream_result) = Http2::get_stream_data(&self.interest,
                                                            &mut self.state, &mut buf[..]) {
            let id = stream_result.id;

            match stream_result.state {
                StreamDataState::Read(info) => {
                    trace!("send_next_data: got bytes for stream {}", id);
                    // Remove stream from interest if it's done providing data
                    let (bytes, end) = match info {
                        ReadInfo::Pending(bytes) => (bytes, EndStream::No),
                        ReadInfo::End(bytes) => {
                            self.interest.remove(&id);
                            (bytes, EndStream::Yes)
                        }
                    };

                    // TODO close stream on 0 bytes
                    if bytes == 0 {
                        return Ok(SendStatus::Nothing);
                    }

                    let mut prioritizer = BufferPrioritizer::new(id, &mut buf[..bytes], end);
                    try!(self.conn.sender(sender).send_next_data(&mut prioritizer));
                    return Ok(SendStatus::Sent);
                },
                // TODO this should only need to handle errors and reads. Nothing is returned in the
                // case of Unavailable.
                StreamDataState::Unavailable => unreachable!(),
                StreamDataState::Error => {
                    try!(self.rst_stream(id, ErrorCode::InternalError, sender));
                },
            }
        }

        Ok(SendStatus::Nothing)
    }

    /// Initiate a stream reset
    ///
    /// Processing the stream identified by `id` has failed locally. Send a RST_STREAM frame to our
    /// peer, remove the stream from the state machine, and run the on_error callback for the
    /// stream.
    fn rst_stream<S: SendFrame>(&mut self,
                                id: StreamId,
                                code: ErrorCode,
                                sender: &mut S) -> HttpResult<()>
    {
        // Send RST_STREAM frame
        try!(self.conn.sender(sender).rst_stream(id, code));

        // Don't try and stream data
        self.interest.remove(&id);

        // Remove stream from the session state
        if let Some(mut stream) = self.state.remove_stream(id) {
            // Run the on_error callback saying that is closed due to error returnd from the
            // handler.
            stream.on_error(super::RequestError::User);
        }

        Ok(())
    }

    fn get_stream_data(interest: &HashSet<StreamId>,
                       state: &mut DefaultSessionState<session::Client, Stream>,
                       buf: &mut [u8]) -> Option<StreamResult> {
        interest.iter()
            .map(|id| {
                let stream = state.get_stream_mut(*id).expect("interest ∈ state");
                debug_assert!(!stream.is_closed_local());
                StreamResult {
                    id: *id,
                    state: stream.stream_data(buf),
                }
            })
            .find(|res| res.state != StreamDataState::Unavailable)
    }

    pub fn initialize(&mut self, mut conn: connection::Ref) {
        // Write preface
        let mut buf = Vec::new();
        write_preface(&mut buf).expect("infallible write to vec");
        conn.queue_frame(buf);
    }

    pub fn wants_write(&self) -> bool {
        !self.interest.is_empty()
    }
}

impl Protocol for Http2 {
    fn new() -> Http2 {
        let raw_conn = HttpConnection::new(HttpScheme::Http);
        let state = session::default_client_state();

        Http2 {
            conn: raw_conn,
            settings: Default::default(),
            got_settings: false,
            state: state,
            interest: HashSet::new(),
        }
    }

    fn on_data(&mut self, buf: &[u8], mut conn: connection::Ref) -> HttpResult<usize> {
        trace!("on_data");
        let mut total_consumed = 0;
        loop {
            if let Some(mut receiver) = WrappedReceive::parse(&buf[total_consumed..]) {
                let len = receiver.frame().len();
                debug!("Handling an HTTP/2 frame of total size {}", len);

                if !self.got_settings {
                    {
                        let mut sender = SendDirect { conn: &mut conn };
                        // TODO protocol error if this errors
                        try!(self.expect_settings(&mut receiver, &mut sender));
                    }

                    self.got_settings = true;
                    conn.connection_ready();
                } else {
                    let mut sender = SendDirect { conn: &mut conn };
                    try!(self.handle_next_frame(&mut receiver, &mut sender));
                }

                total_consumed += len;
            } else {
                break;
            }
        }

        Ok(total_consumed)
    }

    fn ready_write(&mut self, mut conn: connection::Ref) -> HttpResult<SendStatus> {
        // TODO See about giving it only a reference to some parts of the connection
        // (perhaps conveniently wrapped in some helper wrapper) instead of the
        // full Conn. In fact, that is probably a must, as the protocol would like
        // to have a reference to the event loop too, which the Connection currently
        // does not and should not have (as it is passed as a parameter). The proto
        // could use the ref to the evtloop so that it can dispatch messages to it,
        // perhaps even asynchronously.

        let mut sender = SendDirect { conn: &mut conn };
        self.send_next_data(&mut sender)
    }

    fn notify(&mut self, msg: Msg, mut conn: connection::Ref) -> HttpResult<()> {
        // A sender is needed for all message variants
        let mut sender = SendDirect { conn: &mut conn };
        match msg {
            Msg::CreateStream(request, handler) => {
                let stream = Http2::new_stream(request, handler, self.conn.scheme);
                try!(self.start_request(stream, &mut sender));
            },
            Msg::Ping => {
                try!(self.send_ping(&mut sender));
            }
        }

        Ok(())
    }
}

pub trait RequestDelegate: session::Stream {
    fn started(&mut self, stream_id: StreamId);
}

/// An implementation of the `Session` trait which wraps the Http2 protocol object
///
/// While handling the events signaled by the `HttpConnection`, the struct will modify the given
/// session state appropriately.
///
/// The purpose of the type is to make it easier for client implementations to
/// only handle stream-level events by providing a `Stream` implementation,
/// instead of having to implement all session management callbacks.
///
/// For example, by varying the `Stream` implementation it is easy to implement
/// a client that streams responses directly into a file on the local file system,
/// instead of keeping it in memory (like the `DefaultStream` does), without
/// having to change any HTTP/2-specific logic.
struct ClientSession<'a, G, S>
    where S: SendFrame + 'a,
          G: Settings + 'a,
{
    state: &'a mut DefaultSessionState<session::Client, Stream>,
    sender: &'a mut S,
    settings: &'a mut G,
}

impl<'a, G, S> ClientSession<'a, G, S>
    where S: SendFrame + 'a,
          G: Settings + 'a,
{
    /// Returns a new `ClientSession` associated to the given state.
    #[inline]
    pub fn new(state: &'a mut DefaultSessionState<session::Client, Stream>,
               sender: &'a mut S,
               settings: &'a mut G) -> ClientSession<'a, G, S>
    {
        ClientSession {
            state: state,
            sender: sender,
            settings: settings,
        }
    }
}

impl<'a, G, S> session::Session for ClientSession<'a, G, S>
    where S: SendFrame + 'a,
          G: Settings + 'a,
{
    fn new_data_chunk(&mut self,
                      stream_id: StreamId,
                      data: &[u8],
                      _: &mut HttpConnection)
                      -> HttpResult<()>
    {
        debug!("Data chunk for stream {}", stream_id);
        let mut stream = match self.state.get_stream_mut(stream_id) {
            None => {
                debug!("Received a frame for an unknown stream!");
                // TODO(mlalic): This can currently indicate two things:
                //                 1) the stream was idle => PROTOCOL_ERROR
                //                 2) the stream was closed => STREAM_CLOSED (stream error)
                return Ok(());
            }
            Some(stream) => stream,
        };
        // Now let the stream handle the data chunk
        stream.new_data_chunk(data);
        Ok(())
    }

    fn new_headers<'n, 'v>(&mut self,
                           stream_id: StreamId,
                           headers: Vec<Header<'n, 'v>>,
                           conn: &mut HttpConnection) -> HttpResult<()>
    {
        debug!("Headers for stream {}", stream_id);
        let (status_code, headers) = match header::to_status_and_headers(headers) {
            Ok(vals) => vals,
            Err(_err) => {
                // Headers not provided according to HTTP protocol; reset the stream.
                try!(self.rst_stream(stream_id, ErrorCode::ProtocolError, conn));
                return Ok(());
            },
        };

        // Handle stream not being available
        if let Some(mut stream) = self.state.get_stream_mut(stream_id) {
            stream.on_response(Response {
                headers: headers,
                status: status_code,
            });
        }

        Ok(())
    }

    fn end_of_stream(&mut self, stream_id: StreamId, _: &mut HttpConnection) -> HttpResult<()> {
        debug!("End of stream {}", stream_id);
        let mut stream = match self.state.get_stream_mut(stream_id) {
            None => {
                debug!("Received a frame for an unknown stream!");
                return Ok(());
            }
            Some(stream) => stream,
        };
        // Since this implies that the server has closed the stream (i.e. provided a response), we
        // close the local end of the stream, as well as the remote one; there's no need to keep
        // sending out the request body if the server's decided that it doesn't want to see it.
        stream.close();
        Ok(())
    }

    fn rst_stream(&mut self,
                  stream_id: StreamId,
                  error_code: ErrorCode,
                  _: &mut HttpConnection) -> HttpResult<()>
    {
        debug!("RST_STREAM id={:?}, error={:?}", stream_id, error_code);
        self.state.get_stream_mut(stream_id).map(|stream| stream.on_rst_stream(error_code));
        Ok(())
    }

    fn new_settings(&mut self,
                    settings: Vec<HttpSetting>,
                    conn: &mut HttpConnection) -> HttpResult<()>
    {
        debug!("Sending a SETTINGS ack");

        for setting in settings {
            if let HttpSetting::MaxConcurrentStreams(val) = setting {
                self.settings.set_max_concurrent_streams(val);
            }
        }

        conn.sender(self.sender).send_settings_ack()
    }

    fn on_ping(&mut self, ping: &frame::PingFrame, conn: &mut HttpConnection) -> HttpResult<()> {
        debug!("Sending a PING ack");
        conn.sender(self.sender).send_ping_ack(ping.opaque_data())
    }

    fn on_pong(&mut self, _ping: &frame::PingFrame, _conn: &mut HttpConnection) -> HttpResult<()> {
        // TODO need to call the connection handler on_pong function
        debug!("Received a PING ack");
        Ok(())
    }

    /// Handle a GOAWAY frame sent from the server
    ///
    /// The `last_stream_id` indicates the last stream which the client initiated that has been or
    /// may yet be processed by the server. Streams with an ID greater than this will immedately be
    /// removed from the state object and have their on_error handler called with a
    /// RequestError::GoAwayUnprocessed(code). Streams with an ID less than the `last_stream_id`
    /// will have their on_error handler called with RequestError::GoAwayMaybeProcessed(code).
    /// Finally, an HttpError is returned from this handler which should bubble up all the way to
    /// the connection level.
    fn on_goaway(&mut self,
                 last_stream_id: StreamId,
                 error_code: ErrorCode,
                 debug_data: Option<&[u8]>,
                 _conn: &mut HttpConnection) -> HttpResult<()>
    {
        use ::solicit::http::ConnectionError;

        for (id, stream) in self.state.iter() {
            if *id > last_stream_id {
                // Definitely unprocessed
                stream.on_error(RequestError::GoAwayUnprocessed(error_code));
            } else {
                // Maybe processed
                stream.on_error(RequestError::GoAwayMaybeProcessed(error_code));
            }
        }

        let err = match debug_data {
            Some(data) => ConnectionError::with_debug_data(error_code, data.to_vec()),
            None => ConnectionError::new(error_code),
        };

        Err(HttpError::PeerConnectionError(err))
    }
}
