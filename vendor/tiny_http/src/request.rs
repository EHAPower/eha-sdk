use std::io::Error as IoError;
use std::io::{self, Cursor, ErrorKind, Read, Write};

use std::fmt;
use std::net::SocketAddr;
use std::str::FromStr;

use std::sync::mpsc::Sender;

use crate::util::{EqualReader, FusedReader};
use crate::{HTTPVersion, Header, Method, Response, StatusCode};
use chunked_transfer::Decoder;

/// Represents an HTTP request made by a client.
///
/// A `Request` object is what is produced by the server, and is your what
/// your code must analyse and answer.
///
/// This object implements the `Send` trait, therefore you can dispatch your requests to
/// worker threads.
///
/// # Pipelining
///
/// If a client sends multiple requests in a row (without waiting for the response), then you will
/// get multiple `Request` objects simultaneously. This is called *requests pipelining*.
/// Tiny-http automatically reorders the responses so that you don't need to worry about the order
/// in which you call `respond` or `into_writer`.
///
/// This mechanic is disabled if:
///
///  - The body of a request is large enough (handling requires pipelining requires storing the
///    body of the request in a buffer ; if the body is too big, tiny-http will avoid doing that)
///  - A request sends a `Expect: 100-continue` header (which means that the client waits to
///    know whether its body will be processed before sending it)
///  - A request sends a `Connection: close` header or `Connection: upgrade` header (used for
///    websockets), which indicates that this is the last request that will be received on this
///    connection
///
/// # Automatic cleanup
///
/// If a `Request` object is destroyed without `into_writer` or `respond` being called,
/// an empty response with a 500 status code (internal server error) will automatically be
/// sent back to the client.
/// This means that if your code fails during the handling of a request, this "internal server
/// error" response will automatically be sent during the stack unwinding.
///
/// # Testing
///
/// If you want to build fake requests to test your server, use [`TestRequest`](crate::test::TestRequest).
/// Cancellation control for one request. Aborting shuts down its actual connection,
/// including other pipelined requests, and wakes blocked body readers and writers.
#[derive(Clone)]
pub struct RequestAbortHandle {
    connection: std::sync::Arc<std::sync::Mutex<crate::util::refined_tcp_stream::Stream>>,
    completed: std::sync::Arc<std::sync::atomic::AtomicBool>,
    connection_closed: std::sync::Arc<std::sync::atomic::AtomicBool>,
}
impl RequestAbortHandle {
    pub fn abort(&self) {
        use std::sync::atomic::Ordering;
        if self.completed.load(Ordering::Acquire) {
            return;
        }
        self.connection_closed.store(true, Ordering::Release);
        // This lock protects only shutdown handles; normal socket I/O never holds it.
        let mut connection = self.connection.lock().unwrap_or_else(|e| e.into_inner());
        let _ = connection.shutdown(std::net::Shutdown::Both);
    }
    pub fn is_completed(&self) -> bool {
        self.completed.load(std::sync::atomic::Ordering::Acquire)
    }
    pub fn is_aborted(&self) -> bool {
        self.connection_closed
            .load(std::sync::atomic::Ordering::Acquire)
    }
}

pub struct Request {
    abort_handle: Option<RequestAbortHandle>,
    bounded: bool,
    // where to read the body from
    data_reader: Option<Box<dyn Read + Send + 'static>>,

    // if this writer is empty, then the request has been answered
    response_writer: Option<Box<dyn Write + Send + 'static>>,

    remote_addr: Option<SocketAddr>,

    // true if HTTPS, false if HTTP
    secure: bool,

    method: Method,

    path: String,

    http_version: HTTPVersion,

    headers: Vec<Header>,

    body_length: Option<usize>,

    // true if a `100 Continue` response must be sent when `as_reader()` is called
    must_send_continue: bool,

    // If Some, a message must be sent after responding
    notify_when_responded: Option<Sender<()>>,
}

struct NotifyOnDrop<R> {
    sender: Sender<()>,
    inner: R,
}

impl<R: Read> Read for NotifyOnDrop<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}
impl<R: Write> Write for NotifyOnDrop<R> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}
impl<R> Drop for NotifyOnDrop<R> {
    fn drop(&mut self) {
        self.sender.send(()).unwrap();
    }
}

/// Error that can happen when building a `Request` object.
#[derive(Debug)]
pub enum RequestCreationError {
    /// The client sent an `Expect` header that was not recognized by tiny-http.
    ExpectationFailed,

    /// Error while reading data from the socket during the creation of the `Request`.
    CreationIoError(IoError),
}

impl From<IoError> for RequestCreationError {
    fn from(err: IoError) -> RequestCreationError {
        RequestCreationError::CreationIoError(err)
    }
}

/// Builds a new request.
///
/// After the request line and headers have been read from the socket, a new `Request` object
/// is built.
///
/// You must pass a `Read` that will allow the `Request` object to read from the incoming data.
/// It is the responsibility of the `Request` to read only the data of the request and not further.
///
/// The `Write` object will be used by the `Request` to write the response.
#[allow(clippy::too_many_arguments)]
pub fn new_request<R, W>(
    secure: bool,
    method: Method,
    path: String,
    version: HTTPVersion,
    headers: Vec<Header>,
    remote_addr: Option<SocketAddr>,
    source_data: R,
    writer: W,
) -> Result<Request, RequestCreationError>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    new_request_with_wire_limit(
        secure,
        method,
        path,
        version,
        headers,
        remote_addr,
        source_data,
        writer,
        None,
    )
}

// A framing budget is an error, not EOF: a truncated chunk may contain valid JSON.
struct WireBudgetReader<R> {
    source: R,
    remaining: usize,
}
impl<R: Read> Read for WireBudgetReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            return Err(IoError::new(
                ErrorKind::InvalidData,
                "Encoded chunked body exceeds byte limit",
            ));
        }
        let length = self.remaining.min(buf.len());
        let read = self.source.read(&mut buf[..length])?;
        self.remaining -= read;
        Ok(read)
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn new_request_with_wire_limit<R, W>(
    secure: bool,
    method: Method,
    path: String,
    version: HTTPVersion,
    headers: Vec<Header>,
    remote_addr: Option<SocketAddr>,
    mut source_data: R,
    writer: W,
    body_wire_limit: Option<usize>,
) -> Result<Request, RequestCreationError>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    // finding the transfer-encoding header
    let transfer_encoding = headers
        .iter()
        .find(|h: &&Header| h.field.equiv("Transfer-Encoding"))
        .map(|h| h.value.clone());

    // finding the content-length header
    let content_length = if transfer_encoding.is_some() {
        // if transfer-encoding is specified, the Content-Length
        // header must be ignored (RFC2616 #4.4)
        None
    } else {
        headers
            .iter()
            .find(|h: &&Header| h.field.equiv("Content-Length"))
            .and_then(|h| FromStr::from_str(h.value.as_str()).ok())
    };

    // true if the client sent a `Expect: 100-continue` header
    let expects_continue = {
        match headers
            .iter()
            .find(|h: &&Header| h.field.equiv("Expect"))
            .map(|h| h.value.as_str())
        {
            None => false,
            Some(v) if v.eq_ignore_ascii_case("100-continue") => true,
            _ => return Err(RequestCreationError::ExpectationFailed),
        }
    };

    // true if the client sent a `Connection: upgrade` header
    let connection_upgrade = {
        match headers
            .iter()
            .find(|h: &&Header| h.field.equiv("Connection"))
            .map(|h| h.value.as_str())
        {
            Some(v) if v.to_ascii_lowercase().contains("upgrade") => true,
            _ => false,
        }
    };

    // we wrap `source_data` around a reading whose nature depends on the transfer-encoding and
    // content-length headers
    let reader = if connection_upgrade {
        // if we have a `Connection: upgrade`, always keeping the whole reader
        Box::new(source_data) as Box<dyn Read + Send + 'static>
    } else if let Some(content_length) = content_length {
        if content_length == 0 {
            Box::new(io::empty()) as Box<dyn Read + Send + 'static>
        } else if content_length <= 1024 && !expects_continue {
            // if the content-length is small enough, we just read everything into a buffer

            let mut buffer = vec![0; content_length];
            let mut offset = 0;

            while offset != content_length {
                let read = source_data.read(&mut buffer[offset..])?;
                if read == 0 {
                    // the socket returned EOF, but we were before the expected content-length
                    // aborting
                    let info = "Connection has been closed before we received enough data";
                    let err = IoError::new(ErrorKind::ConnectionAborted, info);
                    return Err(RequestCreationError::CreationIoError(err));
                }

                offset += read;
            }

            Box::new(Cursor::new(buffer)) as Box<dyn Read + Send + 'static>
        } else {
            let (data_reader, _) = EqualReader::new(source_data, content_length); // TODO:
            Box::new(FusedReader::new(data_reader)) as Box<dyn Read + Send + 'static>
        }
    } else if transfer_encoding.is_some() {
        // if a transfer-encoding was specified, then "chunked" is ALWAYS applied
        // over the message (RFC2616 #3.6)
        // Bound raw framing before the decoder: decoded-byte caps do not constrain size lines.
        let source: Box<dyn Read + Send> = match body_wire_limit {
            Some(limit) => Box::new(WireBudgetReader {
                source: source_data,
                remaining: limit,
            }),
            None => Box::new(source_data),
        };
        Box::new(FusedReader::new(Decoder::new(source))) as Box<dyn Read + Send + 'static>
    } else {
        // if we have neither a Content-Length nor a Transfer-Encoding,
        // assuming that we have no data
        // TODO: could also be multipart/byteranges
        Box::new(io::empty()) as Box<dyn Read + Send + 'static>
    };

    Ok(Request {
        abort_handle: None,
        bounded: false,
        data_reader: Some(reader),
        response_writer: Some(Box::new(writer) as Box<dyn Write + Send + 'static>),
        remote_addr,
        secure,
        method,
        path,
        http_version: version,
        headers,
        body_length: content_length,
        must_send_continue: expects_continue,
        notify_when_responded: None,
    })
}

impl Request {
    pub(crate) fn with_bounded(mut self, bounded: bool) -> Self {
        self.bounded = bounded;
        self
    }
    pub(crate) fn with_abort_handle(
        mut self,
        connection: std::sync::Arc<std::sync::Mutex<crate::util::refined_tcp_stream::Stream>>,
        connection_closed: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        self.abort_handle = Some(RequestAbortHandle {
            connection,
            completed: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            connection_closed,
        });
        self
    }

    pub fn abort_handle(&self) -> Option<RequestAbortHandle> {
        self.abort_handle.clone()
    }

    /// Close the real socket before dropping its body reader. No response or body drain
    /// may wait for the peer after shutdown; the request is never replayed.
    pub fn abort(mut self) {
        if let Some(handle) = &self.abort_handle {
            handle.abort();
        }
        drop(self.response_writer.take());
    }

    /// Respond with Connection: close, then shut down the socket before disposing of
    /// an unread body. Keep-alive/pipelining are intentionally disabled for this response.
    pub fn respond_and_close<R: Read>(mut self, mut response: Response<R>) -> Result<(), IoError> {
        response.close_connection();
        let result = self.respond_impl(response);
        if let Some(handle) = &self.abort_handle {
            handle.abort();
        }
        if let Some(sender) = self.notify_when_responded.take() {
            let _ = sender.send(());
        }
        result
    }

    /// Returns true if the request was made through HTTPS.
    #[inline]
    pub fn secure(&self) -> bool {
        self.secure
    }

    /// Returns the method requested by the client (eg. `GET`, `POST`, etc.).
    #[inline]
    pub fn method(&self) -> &Method {
        &self.method
    }

    /// Returns the resource requested by the client.
    #[inline]
    pub fn url(&self) -> &str {
        &self.path
    }

    /// Returns a list of all headers sent by the client.
    #[inline]
    pub fn headers(&self) -> &[Header] {
        &self.headers
    }

    /// Returns the HTTP version of the request.
    #[inline]
    pub fn http_version(&self) -> &HTTPVersion {
        &self.http_version
    }

    /// Returns the length of the body in bytes.
    ///
    /// Returns `None` if the length is unknown.
    #[inline]
    pub fn body_length(&self) -> Option<usize> {
        self.body_length
    }

    /// Returns the address of the client that sent this request.
    ///
    /// The address is always `Some` for TCP listeners, but always `None` for UNIX listeners
    /// (as the remote address of a UNIX client is almost always unnamed).
    ///
    /// Note that this is gathered from the socket. If you receive the request from a proxy,
    /// this function will return the address of the proxy and not the address of the actual
    /// user.
    #[inline]
    pub fn remote_addr(&self) -> Option<&SocketAddr> {
        self.remote_addr.as_ref()
    }

    /// Sends a response with a `Connection: upgrade` header, then turns the `Request` into a `Stream`.
    ///
    /// The main purpose of this function is to support websockets.
    /// If you detect that the request wants to use some kind of protocol upgrade, you can
    ///  call this function to obtain full control of the socket stream.
    ///
    /// If you call this on a non-websocket request, tiny-http will wait until this `Stream` object
    ///  is destroyed before continuing to read or write on the socket. Therefore you should always
    ///  destroy it as soon as possible.
    pub fn upgrade<R: Read>(
        mut self,
        protocol: &str,
        response: Response<R>,
    ) -> Box<dyn ReadWrite + Send> {
        use crate::util::CustomStream;

        response
            .raw_print(
                self.response_writer.as_mut().unwrap().by_ref(),
                self.http_version.clone(),
                &self.headers,
                false,
                Some(protocol),
            )
            .ok(); // TODO: unused result

        self.response_writer.as_mut().unwrap().flush().ok(); // TODO: unused result

        let stream = CustomStream::new(self.extract_reader_impl(), self.extract_writer_impl());
        if let Some(sender) = self.notify_when_responded.take() {
            let stream = NotifyOnDrop {
                sender,
                inner: stream,
            };
            Box::new(stream) as Box<dyn ReadWrite + Send>
        } else {
            Box::new(stream) as Box<dyn ReadWrite + Send>
        }
    }

    /// Allows to read the body of the request.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # extern crate rustc_serialize;
    /// # extern crate tiny_http;
    /// # use rustc_serialize::json::Json;
    /// # use std::io::Read;
    /// # fn get_content_type(_: &tiny_http::Request) -> &'static str { "" }
    /// # fn main() {
    /// # let server = tiny_http::Server::http("0.0.0.0:0").unwrap();
    /// let mut request = server.recv().unwrap();
    ///
    /// if get_content_type(&request) == "application/json" {
    ///     let mut content = String::new();
    ///     request.as_reader().read_to_string(&mut content).unwrap();
    ///     let json: Json = content.parse().unwrap();
    /// }
    /// # }
    /// ```
    ///
    /// If the client sent a `Expect: 100-continue` header with the request, calling this
    ///  function will send back a `100 Continue` response.
    #[inline]
    pub fn as_reader(&mut self) -> &mut dyn Read {
        if self.must_send_continue {
            let msg = Response::new_empty(StatusCode(100));
            msg.raw_print(
                self.response_writer.as_mut().unwrap().by_ref(),
                self.http_version.clone(),
                &self.headers,
                true,
                None,
            )
            .ok();
            self.response_writer.as_mut().unwrap().flush().ok();
            self.must_send_continue = false;
        }

        self.data_reader.as_mut().unwrap()
    }

    /// Turns the `Request` into a writer.
    ///
    /// The writer has a raw access to the stream to the user.
    /// This function is useful for things like CGI.
    ///
    /// Note that the destruction of the `Writer` object may trigger
    /// some events. For exemple if a client has sent multiple requests and the requests
    /// have been processed in parallel, the destruction of a writer will trigger
    /// the writing of the next response.
    /// Therefore you should always destroy the `Writer` as soon as possible.
    #[inline]
    pub fn into_writer(mut self) -> Box<dyn Write + Send + 'static> {
        let writer = self.extract_writer_impl();
        if let Some(sender) = self.notify_when_responded.take() {
            let writer = NotifyOnDrop {
                sender,
                inner: writer,
            };
            Box::new(writer) as Box<dyn Write + Send + 'static>
        } else {
            writer
        }
    }

    /// Extract the response `Writer` object from the Request, dropping this `Writer` has the same side effects
    /// as the object returned by `into_writer` above.
    ///
    /// This may only be called once on a single request.
    fn extract_writer_impl(&mut self) -> Box<dyn Write + Send + 'static> {
        use std::mem;

        assert!(self.response_writer.is_some());

        let mut writer = None;
        mem::swap(&mut self.response_writer, &mut writer);
        writer.unwrap()
    }

    /// Extract the body `Reader` object from the Request.
    ///
    /// This may only be called once on a single request.
    fn extract_reader_impl(&mut self) -> Box<dyn Read + Send + 'static> {
        use std::mem;

        assert!(self.data_reader.is_some());

        let mut reader = None;
        mem::swap(&mut self.data_reader, &mut reader);
        reader.unwrap()
    }

    /// Sends a response to this request.
    #[inline]
    pub fn respond<R>(mut self, response: Response<R>) -> Result<(), IoError>
    where
        R: Read,
    {
        let res = self.respond_impl(response);
        if let Some(sender) = self.notify_when_responded.take() {
            sender.send(()).unwrap();
        }
        res
    }

    fn respond_impl<R>(&mut self, response: Response<R>) -> Result<(), IoError>
    where
        R: Read,
    {
        let mut writer = self.extract_writer_impl();

        let do_not_send_body = self.method == Method::Head;

        Self::ignore_client_closing_errors(response.raw_print(
            writer.by_ref(),
            self.http_version.clone(),
            &self.headers,
            do_not_send_body,
            None,
        ))?;

        Self::ignore_client_closing_errors(writer.flush())
    }

    fn ignore_client_closing_errors(result: io::Result<()>) -> io::Result<()> {
        result.or_else(|err| match err.kind() {
            ErrorKind::BrokenPipe => Ok(()),
            ErrorKind::ConnectionAborted => Ok(()),
            ErrorKind::ConnectionRefused => Ok(()),
            ErrorKind::ConnectionReset => Ok(()),
            _ => Err(err),
        })
    }

    pub(crate) fn with_notify_sender(mut self, sender: Sender<()>) -> Self {
        self.notify_when_responded = Some(sender);
        self
    }
}

impl fmt::Debug for Request {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        write!(
            formatter,
            "Request({} {} from {:?})",
            self.method, self.path, self.remote_addr
        )
    }
}

impl Drop for Request {
    fn drop(&mut self) {
        if self.bounded {
            if let Some(handle) = &self.abort_handle {
                handle.abort();
            }
            drop(self.response_writer.take());
        }
        if self.response_writer.is_some() {
            let response = Response::empty(500);
            let _ = self.respond_impl(response); // ignoring any potential error
            if let Some(sender) = self.notify_when_responded.take() {
                sender.send(()).unwrap();
            }
        }
        // Completion is published only after body cleanup. A deadline can therefore
        // still abort a socket while EqualReader::drop is draining it.
        drop(self.data_reader.take());
        if let Some(handle) = &self.abort_handle {
            handle
                .completed
                .store(true, std::sync::atomic::Ordering::Release);
        }
    }
}

/// Dummy trait that regroups the `Read` and `Write` traits.
///
/// Automatically implemented on all types that implement both `Read` and `Write`.
pub trait ReadWrite: Read + Write {}
impl<T> ReadWrite for T where T: Read + Write {}

#[cfg(test)]
mod tests {
    use super::Request;

    fn wire_request(body: Vec<u8>, limit: usize) -> Request {
        super::new_request_with_wire_limit(
            false,
            crate::Method::Post,
            "/".into(),
            crate::HTTPVersion(1, 1),
            vec![crate::Header::from_bytes("Transfer-Encoding", "chunked").unwrap()],
            None,
            std::io::Cursor::new(body),
            std::io::sink(),
            Some(limit),
        )
        .unwrap()
    }
    #[test]
    fn wire_budget_truncated_chunk_cannot_succeed_with_complete_json_prefix() {
        use std::io::Read;
        let prefix = b"10;long-extension\r\n{}";
        let mut body = prefix.to_vec();
        body.extend_from_slice(b"extra-padding!\r\n0\r\n\r\n");
        let mut request = wire_request(body, prefix.len());
        let mut decoded = String::new();
        let error = request
            .as_reader()
            .read_to_string(&mut decoded)
            .unwrap_err();
        assert_eq!(decoded, "{}");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }
    #[test]
    fn wire_budget_allows_valid_chunked_end_exactly_at_limit() {
        use std::io::Read;
        let body = b"2\r\n{}\r\n0\r\n\r\n".to_vec();
        let limit = body.len();
        let mut request = wire_request(body, limit);
        let mut decoded = String::new();
        request.as_reader().read_to_string(&mut decoded).unwrap();
        assert_eq!(decoded, "{}");
        let mut byte = [0];
        assert_eq!(request.as_reader().read(&mut byte).unwrap(), 0);
    }

    #[test]
    fn must_be_send() {
        #![allow(dead_code)]
        fn f<T: Send>(_: &T) {}
        fn bar(rq: &Request) {
            f(rq);
        }
    }
}
