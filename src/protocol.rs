use std::sync::mpsc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::io::{Cursor, Read};

use solicit::http::frame::{Frame, RawFrame, FrameIR, HttpSetting};
use solicit::http::{HttpScheme, HttpResult, Header, StreamId};
use solicit::http::connection::{HttpConnection, SendFrame, ReceiveFrame, HttpFrame};
use solicit::http::client::{write_preface, ClientConnection, RequestStream};
use solicit::http::session::{DefaultSessionState, SessionState, DefaultStream, StreamState};
use solicit::http::session::{self, StreamDataChunk, StreamDataError};

use httparse;

/// Stream events and Request/Response bytes are delivered to/from the handler.
pub trait StreamHandler: Send + 'static {
    /// Provide data from the request body
    fn get_data_chunk(&mut self, buf: &mut [u8]) -> Result<StreamDataChunk, StreamDataError>;

    /// Response headers are available
    fn on_response_headers(&mut self, res: Headers);

    /// Data from the response is available
    fn on_response_data(&mut self, data: &[u8]);

    /// Called when the stream is closed
    fn on_close(&mut self);

    /// Error occurred
    fn on_error(&mut self, err: RequestError);
}

pub trait Protocol: Sized + 'static {
    fn new() -> Self;
    fn on_data(&mut self, buf: &[u8], conn: ConnectionRef) -> Result<usize, ()>;
    fn ready_write(&mut self, conn: ConnectionRef);
    fn notify(&mut self, msg: Self::Message, conn: ConnectionRef);
}

/// Messages for an HTTP/2 state machine
pub enum Msg {
    CreateStream(::Request, Box<StreamHandler>),
    Ping,
}

/// SendFrame implementor that pushes onto the connection outgoing frame list.
struct SendDirect<'brw, 'conn> where 'conn: 'brw {
    conn: &'brw mut ConnectionRef<'conn, Http2>,
}

impl<'a, 'b> SendFrame for SendDirect<'a, 'b> {
    fn send_frame<F: FrameIR>(&mut self, frame: F) -> HttpResult<()> {
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
    frame: Option<RawFrame<'a>>,
}

impl<'a> WrappedReceive<'a> {
    /// Attempts to read an entire frame from the transport read buffer.
    ///
    /// In the case where not enough bytes are available to construct a RawFrame, parse returns
    /// None.
    fn parse(buf: &'a [u8]) -> Option<WrappedReceive<'a>> {
        RawFrame::parse(buf).map(|frame| WrappedReceive {
            frame: Some(frame),
        })
    }

    pub fn frame(&self) -> Option<&RawFrame<'a>> {
        self.frame.as_ref()
    }
}

impl<'a> ReceiveFrame for WrappedReceive<'a> {
    /// Take the wrapped frame and parse it
    ///
    /// # Panics
    ///
    /// Panics if called more than once for a given WrappedReceive instance
    ///
    /// # TODO
    ///
    /// HttpFrame should also allow for borrowed frame buffers. As it stands, at this point we have
    /// to make a copy!!! This is not the fault of the ReceiveFrame abstraction, though. The same
    /// would happen if the HttpConn itself parsed a raw buffer into a frame, since it'd need to
    /// create an HttpFrame at some point in order to correspondingly handle it...
    fn recv_frame(&mut self) -> HttpResult<HttpFrame> {
        HttpFrame::from_raw(self.frame.take().unwrap())
    }
}

struct Stream {
    state: session::StreamState,
    inner: Box<StreamHandler>,
}

impl Stream {
    pub fn new(handler: Box<StreamHandler>) -> Stream {
        Stream {
            // TODO should really start in Idle state
            state: session::StreamState::Open,
            inner: handler,
        }
    }
}

impl session::Stream for Stream {
    fn new_data_chunk(&mut self, data: &[u8]) {
        self.inner.on_response_data(data)
    }

    fn set_headers(&mut self, headers: Vec<Header>) {
        let httparse_headers = headers.map(|header| {
            httparse::Header {
                name: str::from_utf8(&header.0[..]).unwrap(),
                value: &header.1[..],
            }
        }).collect::<Vec<_>>();

        // TODO from_raw does a copy of the httparse::Header values. We can provide owned values,
        // so the copies are wasteful.
        let headers = Headers::from_raw(&httpparse_headers[..]).unwrap();

        self.inner.on_response_headers(headers)
    }

    fn set_state(&mut self, state: StreamState) {
        self.state = state;
        if state == StreamState::Closed {
            self.inner.on_close();
        }
    }

    fn get_data_chunk(&mut self, buf: &mut [u8]) -> Result<StreamDataChunk, StreamDataError> {
        self.inner.get_data_chunk(buf)
    }

    /// Returns the current state of the stream.
    fn state(&self) -> StreamState {
        self.state
    }
}

pub struct Http2 {
    init: bool,
    got_settings: bool,
    conn: HttpConnection,
    state: DefaultSessionState<BoxStream>,

    /// Dynamic protocol configuration
    settings: Http2Settings,
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
    pub fn set_max_concurrent_streams(&mut self, val: u32) {
        self.max_concurrent_streams = val;
    }
}

/// Defines interactions with an object holding connection settings.
///
/// Methods on the Settings type are used only when a SETTINGS frame is received for the associated
/// connection.
trait Settings {
    /// Set the maximum number of concurrent streams
    fn set_max_concurrent_streams(&mut self, val: u32);
}

impl Http2 {
    /// Create a RequestStream given a request and handler
    ///
    /// This is a convenience method for generating a RequestStream. It is not added to the
    /// protocol's active streams; that is up to the caller.
    fn new_stream(request: ::Request, handler: Box<StreamHandler>) -> RequestStream<BoxStream> {
        let ::Request { method, path, headers_only } = request;

        // TODO hyper headers
        let mut headers: Vec<Header> = vec![
            Header::new(b":method", format!("{}", method).into_bytes()),
            Header::new(b":path", path.into_bytes()),
            Header::new(b":authority", &b"http2bin.org"[..]),
            Header::new(b":scheme", self.conn.scheme().as_bytes().to_vec()),
        ];

        let stream = Stream::new(handler);
        if headers_only {
            stream.set_state(StreamState::HalfClosedLocal);
        }

        RequestStream {
            headers: headers,
            stream: Stream::new(handler),
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
                                       mut req: RequestStream<State::Stream>,
                                       sender: &mut S)
                                       -> HttpResult<StreamId> {

        let end_stream = if req.stream.is_closed_local() {
            EndStream::Yes
        } else {
            EndStream::No
        };

        let stream_id = self.state.insert_outgoing(req.stream);
        try!(self.conn.sender(sender).send_headers(req.headers, stream_id, end_stream));

        debug!("CreatedStream {:?}", stream_id);
        Ok(stream_id)
    }

    /// Send a PING
    pub fn send_ping<S: SendFrame>(&mut self) -> HttpResult<()> {
        let mut sender = SendDirect { conn: &mut self.conn };
        try!(self.conn.sender(sender).send_ping(0));
        Ok(())
    }

    /// Fully handles the next incoming frame provided by the given `ReceiveFrame` instance.
    /// Handling a frame may cause changes to the session state exposed by the `ClientConnection`.
    pub fn handle_next_frame<Recv: ReceiveFrame, Sender: SendFrame>(&mut self,
                                                                    rx: &mut Recv,
                                                                    tx: &mut Sender)
                                                                    -> HttpResult<()> {
        let mut session = ClientSession::new(&mut self.state, tx, &mut self.settings);
        self.conn.handle_next_frame(rx, &mut session)
    }

    /// Queues a new DATA frame onto the underlying `SendFrame`.
    ///
    /// Currently, no prioritization of streams is taken into account and which stream's data is
    /// queued cannot be relied on.
    pub fn send_next_data<S: SendFrame>(&mut self, sender: &mut S) -> HttpResult<SendStatus> {
        debug!("Sending next data...");
        // A default "maximum" chunk size of 8 KiB is set on all data frames.
        const MAX_CHUNK_SIZE: usize = 8 * 1024;
        let mut buf = [0; MAX_CHUNK_SIZE];

        let mut prioritizer = SimplePrioritizer::new(&mut self.state, &mut buf);
        self.conn.sender(sender).send_next_data(&mut prioritizer)
    }

    fn initialize(&mut self) {
        // Write preface
        let mut buf = Vec::new();
        write_preface(&mut buf).unwrap();
        conn.queue_frame(buf);
        self.init = true;
    }
}

impl Protocol for Http2 {
    fn new() -> Http2 {
        let raw_conn = HttpConnection::new(HttpScheme::Http);
        let state = session::default_client_state();
        let settings = Http2Settings::new();
        Http2 {
            init: false,
            conn: ClientConnection::with_connection(raw_conn, state),
            settings: settings,
        }
    }

    fn on_data<'a>(&mut self, buf: &[u8], mut conn: ConnectionRef) -> Result<usize, ()> {
        trace!("Http2: Received something back");
        let mut total_consumed = 0;
        loop {
            match WrappedReceive::parse(&buf[total_consumed..]) {
                None => {
                    // No frame available yet. We consume nothing extra and wait for more data to
                    // become available to retry.
                    let done = self.conn.state.get_closed();
                    for stream in done {
                        info!("Got response!");
                    }

                    break;
                },
                Some(mut receiver) => {
                    let len = receiver.frame().len();
                    debug!("Handling an HTTP/2 frame of total size {}", len);

                    let mut sender = SendDirect { conn: &mut conn };

                    if !self.got_settings {
                        try!(self.expect_settings(&mut receiver, &mut sender).map_err(|_| ()));
                        self.got_settings = true;
                        conn.connection_ready();
                    } else {
                        try!(self.handle_next_frame(&mut receiver, &mut sender).map_err(|_| ()));
                    }

                    total_consumed += len;
                },
            }
        }

        Ok(total_consumed)
    }

    fn ready_write(&mut self, mut conn: ConnectionRef<Http2>) {
        // TODO See about giving it only a reference to some parts of the connection
        // (perhaps conveniently wrapped in some helper wrapper) instead of the
        // full Conn. In fact, that is probably a must, as the protocol would like
        // to have a reference to the event loop too, which the Connection currently
        // does not and should not have (as it is passed as a parameter). The proto
        // could use the ref to the evtloop so that it can dispatch messages to it,
        // perhaps even asynchronously.
        trace!("Hello, from HTTP2");
        if !self.init {
            self.initialize();
        } else {
            let mut sender = SendDirect { conn: &mut conn };
            self.send_next_data(&mut sender);
        }
    }

    fn notify(&mut self, msg: Msg, mut conn: ConnectionRef) {
        trace!("Http2 notified: msg={:?}", msg);

        if !self.init {
            self.initialize();
        }

        match msg {
            Msg::CreateStream(request, handler) => {
                let stream = Http2::new_stream(request, handler);
                let mut sender = SendDirect { conn: &mut conn };
                self.start_request(stream, &mut sender);
            },
        }
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
struct ClientSession<'a, State, G, S>
    where State: session::SessionState + 'a,
          S: SendFrame + 'a,
          G: Settings + 'a
{
    state: &'a mut State,
    sender: &'a mut S,
    settings: &'a mut G,
}

impl<'a, State, G, S> ClientSession<'a, State, G, S>
    where State: session::SessionState + 'a,
          S: SendFrame + 'a,
          G: Settings + 'a,
{
    /// Returns a new `ClientSession` associated to the given state.
    #[inline]
    pub fn new(state: &'a mut State,
               sender: &'a mut S,
               settings: &'a mut G) -> ClientSession<'a, State, G, S>
    {
        ClientSession {
            state: state,
            sender: sender,
            settings: settings,
        }
    }
}

impl<'a, State, G, S> session::Session for ClientSession<'a, State, G, S>
    where State: session::SessionState + 'a,
          S: SendFrame + 'a,
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
                           _conn: &mut HttpConnection) -> HttpResult<()>
    {
        debug!("Headers for stream {}", stream_id);
        let mut stream = match self.state.get_stream_mut(stream_id) {
            None => {
                debug!("Received a frame for an unknown stream!");
                // TODO(mlalic): This means that the server's header is not associated to any
                //               request made by the client nor any server-initiated stream (pushed)
                return Ok(());
            }
            Some(stream) => stream,
        };
        // Now let the stream handle the headers
        stream.set_headers(headers);
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

        for setting in &settings {
            if let HttpSetting::MaxConcurrentStreams(val) = setting {
                self.settings.set_max_concurrent_streams(val);
            }
        }

        conn.sender(self.sender).send_settings_ack()
    }

    fn on_ping(&mut self, ping: &PingFrame, conn: &mut HttpConnection) -> HttpResult<()> {
        debug!("Sending a PING ack");
        conn.sender(self.sender).send_ping_ack(ping.opaque_data())
    }

    fn on_pong(&mut self, _ping: &PingFrame, _conn: &mut HttpConnection) -> HttpResult<()> {
        debug!("Received a PING ack");
        Ok(())
    }
}
