use std::sync::mpsc;
use std::sync::Arc;
use std::sync::{AtomicUsize, Ordering};
use std::io::{Cursor, Read};

use solicit::http::frame::{Frame, RawFrame, FrameIR, HttpSetting};
use solicit::http::{HttpScheme, HttpResult, Header, StreamId};
use solicit::http::connection::{HttpConnection, SendFrame, ReceiveFrame, HttpFrame};
use solicit::http::client::{write_preface, ClientConnection, RequestStream};
use solicit::http::session::{DefaultSessionState, SessionState, DefaultStream, StreamState};
use solicit::http::session::{self, StreamDataChunk, StreamDataError, Client as ClientMarker};

use httparse;

pub trait Protocol: Sized + 'static {
    type Message: Send + ::std::fmt::Debug;

    fn new() -> Self;
    fn on_data(&mut self, buf: &[u8], conn: ConnectionRef) -> Result<usize, ()>;
    fn ready_write(&mut self, conn: ConnectionRef);
    fn notify(&mut self, msg: Self::Message, conn: ConnectionRef);
}

/// Messages for an HTTP/2 state machine
pub enum Msg {
    CreateStream(::Request, Box<RequestHandler>),
    Ping,
}

struct FakeSend;
impl SendFrame for FakeSend {
    #[inline]
    fn send_frame<F: FrameIR>(&mut self, frame: F) -> HttpResult<()> {
        trace!("FakeSend::send_frame");
        Ok(())
    }
}

// TODO Replace with a send-the-short-way once `solicit` allows us to!
//      It's the long way because even though the HttpConnection at the point when it uses the
//      SendFrame methods is executing within the event loop (and thus has exclusive ownership of
//      everything), we still send a QueueFrame message to the loop instead of directly invoking
//      the equivalent functionality on the ConnectionRef.
//      Solicit needs to facilitate this by changing the HttpConnection API to not require an
//      owned SendFrame instance, but only one that it gets from the session layer in its
//      handle/send methods (similar to how the session delegate/callbacks are passed).
struct SendLongWay {
    handle: ConnectionHandle<Http2>,
}
impl SendFrame for SendLongWay {
    #[inline]
    fn send_frame<F: FrameIR>(&mut self, frame: F) -> HttpResult<()> {
        trace!("SendLongWay::send_raw_frame");
        let mut buf = Cursor::new(Vec::with_capacity(1024));
        try!(frame.serialize_into(&mut buf));
        self.handle.notify(Message::QueueFrame(buf.into_inner()));
        Ok(())
    }
}

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

struct FakeReceive;
impl ReceiveFrame for FakeReceive {
    fn recv_frame(&mut self) -> HttpResult<HttpFrame> {
        panic!("Should never have been called!");
    }
}

struct WrappedReceive<'a> {
    frame: Option<RawFrame<'a>>,
}

impl<'a> WrappedReceive<'a> {
    fn parse(buf: &'a [u8]) -> Option<WrappedReceive<'a>> {
        RawFrame::parse(buf).map(|frame| WrappedReceive {
            frame: Some(frame),
        })
    }
    pub fn frame(&self) -> Option<&RawFrame<'a>> { self.frame.as_ref() }
}
impl<'a> ReceiveFrame for WrappedReceive<'a> {
    fn recv_frame(&mut self) -> HttpResult<HttpFrame> {
        // TODO HttpFrame should also allow for borrowed frame buffers. As it stands, at this point
        //      we have to make a copy!!! This is not the fault of the ReceiveFrame abstraction,
        //      though. The same would happen if the HttpConn itself parsed a raw buffer into a
        //      frame, since it'd need to create an HttpFrame at some point in order to
        //      correspondingly handle it...
        HttpFrame::from_raw(self.frame.as_ref().unwrap())
    }
}

struct Stream {
    state: session::StreamState,
    inner: Box<RequestHandler>,
}

impl Stream {
    pub fn new(handler: Box<RequestHandler>) -> Stream {
        Stream {
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
    state: DefaultSessionState<ClientMarker, BoxStream>,

    /// Dynamic protocol configuration
    settings: Http2Settings,
}

/// Settings type that exposes some properties via Send/Sync types
pub struct Http2Settings {
    /// The connection ref held by consumers needs knowledge of max_concurrent streams
    shared_max_concurrent_streams: Arc<AtomicUsize>,

    /// Max concurrent streams for local use
    max_concurrent_streams: u32
}

impl Http2Settings {
    pub fn new(max_concurrent_streams: Arc<AtomicUsize>) -> Self {
        Http2Settings {
            max_concurrent_streams: max_concurrent_streams,
        }
    }
}

impl Settings for Http2Settings {
    pub fn set_max_concurrent_streams(&mut self, val: u32) {
        self.shared_max_concurrent_streams.store(val as usize, Ordering::SeqCst);
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
    fn new_stream(&mut self,
                  request: ::Request,
                  handler: Box<RequestHandler>) -> RequestStream<BoxStream>
    {
        // TODO Set ID
        // TODO Figure out when to locally close streams that have no data in order to send just
        //      the headers with a request...
        //

        let ::Request { method, path } = request;

        // TODO hyper headers
        let mut headers: Vec<Header> = vec![
            Header::new(b":method", format!(method).into_bytes()),
            Header::new(b":path", path.into_bytes()),
            Header::new(b":authority", &b"http2bin.org"[..]),
            Header::new(b":scheme", self.conn.scheme().as_bytes().to_vec()),
        ];

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
                                       req: RequestStream<State::Stream>,
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
    type Message = HttpMsg;

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
                    let len = receiver.frame().unwrap().len();
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

    fn notify(&mut self, msg: Msg, mut conn: ConnectionRef<Http2>) {
        trace!("Http2 notified: msg={:?}", msg);

        if !self.init {
            self.initialize();
        }

        match msg {
            Msg::CreateStream(request, handler) => {
                let stream = self.new_stream(request, handler);
                let mut sender = SendDirect { conn: &mut conn };
                self.start_request(stream, &mut sender);
            },
        }
    }
}

pub trait RequestDelegate: session::Stream {
    fn started(&mut self, stream_id: StreamId);
}

impl ::std::fmt::Debug for HttpMsg {
    fn fmt(&self, f: &mut ::std::fmt::Formatter) -> Result<(), ::std::fmt::Error> {
        f.debug_struct("HttpMsg").finish()
    }
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

impl<'a, State, G, S> Session for ClientSession<'a, State, G, S>
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
