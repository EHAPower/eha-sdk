// Copyright The eha-sdk Contributors
//! HTTP 传输截止时间不持有设备会话；真实 socket abort 先于 Request 的 body 析构。

use std::time::{Duration, Instant};
use tiny_http::{Request, RequestAbortHandle};

pub(super) const IO_DEADLINE_MS: u64 = 10_000;
const MAX_PENDING: usize = 64;

#[derive(Default)]
pub(super) struct Deadlines {
    pending: Vec<(Instant, RequestAbortHandle)>,
}
impl Deadlines {
    pub fn register(&mut self, request: &Request) -> bool {
        self.poll();
        if self.pending.len() >= MAX_PENDING {
            return false;
        }
        if let Some(handle) = request.abort_handle() {
            self.pending.push((
                Instant::now() + Duration::from_millis(IO_DEADLINE_MS),
                handle,
            ));
        }
        true
    }
    pub fn poll(&mut self) {
        let now = Instant::now();
        self.pending.retain(|(deadline, handle)| {
            if handle.is_completed() {
                return false;
            }
            if now >= *deadline {
                handle.abort();
                return false;
            }
            true
        });
    }
}
impl Drop for Deadlines {
    fn drop(&mut self) {
        for (_, handle) in &self.pending {
            handle.abort();
        }
    }
}

pub(super) fn was_aborted(request: &Request) -> bool {
    request
        .abort_handle()
        .is_some_and(|handle| handle.is_aborted())
}

#[cfg(test)]
mod tests {
    use super::super::{CanScanView, OdriveView, Workbench, integration::IntegrationState};
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpStream,
        sync::mpsc,
        thread,
    };

    #[test]
    fn more_than_response_capacity_unauthenticated_slow_bodies_do_not_block_owner() {
        let server = super::server(0).expect("server");
        let address = server.server_addr().to_ip().expect("address");
        let (stop_tx, stop_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let mut owner = Workbench::new(std::env::temp_dir());
            owner.v2 = Some(IntegrationState::for_test().for_v2());
            owner.legacy_routes = false;
            let mut odrive = OdriveView::new(None);
            let mut scan = CanScanView::new();
            let reader = super::super::http_body::BodyReader::new();
            let mut deadlines = Deadlines::default();
            let mut ticks = 0;
            while stop_rx.try_recv().is_err() {
                deadlines.poll();
                owner.tick();
                ticks += 1;
                if let Some(request) = server
                    .recv_timeout(Duration::from_millis(2))
                    .expect("request")
                {
                    assert!(deadlines.register(&request));
                    super::super::handle_received_request(
                        request,
                        address.port(),
                        &mut owner,
                        &mut odrive,
                        &mut scan,
                        &reader,
                    )
                    .expect("dispatch");
                }
            }
            ticks
        });
        let mut attackers = Vec::new();
        for _ in 0..20 {
            let mut stream = TcpStream::connect(address).expect("connect");
            stream
                .set_read_timeout(Some(Duration::from_secs(1)))
                .expect("timeout");
            write!(stream,"POST /service/v2/bindings HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Type: application/json\r\nContent-Length: 2048\r\n\r\n",address.port()).expect("headers");
            let mut response = [0; 1024];
            let length = stream
                .read(&mut response)
                .expect("401 without request body");
            assert!(String::from_utf8_lossy(&response[..length]).contains("401"));
            attackers.push(stream);
        }
        let mut client = TcpStream::connect(address).expect("authorized connect");
        client
            .set_read_timeout(Some(Duration::from_millis(600)))
            .expect("timeout");
        write!(client,"GET /service/v2/describe HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer test-token\r\n\r\n",address.port()).expect("describe");
        let mut response = String::new();
        client
            .read_to_string(&mut response)
            .expect("describe remains responsive");
        assert!(response.starts_with("HTTP/1.1 200"));
        stop_tx.send(()).expect("stop test owner");
        assert!(worker.join().expect("worker") >= 21);
        drop(attackers);
    }

    #[test]
    fn abort_wakes_unread_body_and_deadline_cancels_a_slow_upload() {
        let server = tiny_http::Server::http(("127.0.0.1", 0)).expect("server");
        let address = server.server_addr().to_ip().expect("address");
        let mut stream = TcpStream::connect(address).expect("connect");
        write!(
            stream,
            "POST / HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Length: 2048\r\n\r\n",
            address.port()
        )
        .expect("headers");
        let request = server.recv().expect("request");
        let mut deadlines = Deadlines::default();
        assert!(deadlines.register(&request));
        let handle = request.abort_handle().expect("real socket control");
        deadlines.pending[0].0 = Instant::now();
        deadlines.poll();
        assert!(handle.is_aborted());
        let start = Instant::now();
        request.abort();
        assert!(start.elapsed() < Duration::from_millis(100));
        assert!(handle.is_completed());
        assert!(deadlines.pending.is_empty());
    }
}

/// 整体捕获该包装器；spawn 失败析构闭包时先关闭 socket，再析构 body。
pub(super) struct AbortOnDrop(Option<Request>);
impl AbortOnDrop {
    pub fn new(request: Request) -> Self {
        Self(Some(request))
    }
    pub fn respond(mut self, response: tiny_http::Response<std::io::Cursor<Vec<u8>>>) {
        if let Some(request) = self.0.take() {
            let _ = request.respond_and_close(response);
        }
    }
}
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        if let Some(request) = self.0.take() {
            request.abort();
        }
    }
}
pub(super) fn spawn_response<S: Send + 'static>(
    builder: std::thread::Builder,
    request: Request,
    response: tiny_http::Response<std::io::Cursor<Vec<u8>>>,
    slot: S,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    let request = AbortOnDrop::new(request);
    builder.spawn(move || {
        let _slot = slot;
        request.respond(response);
    })
}
pub(super) fn server(port: u16) -> Result<tiny_http::Server, String> {
    tiny_http::Server::http_bounded(
        ("127.0.0.1", port),
        tiny_http::ServerLimits {
            max_connections: 64,
            max_queued_requests: 64,
            max_header_bytes: 16 * 1024,
            max_body_wire_bytes: 8 * 1024 * 1024,
            connection_lifetime: Duration::from_millis(IO_DEADLINE_MS),
        },
    )
    .map_err(|error| error.to_string())
}
pub(super) fn write_ready(writer: &mut impl std::io::Write, ready: &str) {
    // stdout 是发现提示的消费者，不拥有已发布的服务生命周期。
    let _ = writeln!(writer, "{ready}");
}

#[cfg(test)]
mod resource_tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpStream,
        thread,
    };
    fn bounded(connections: usize, queue: usize, lifetime: u64) -> tiny_http::Server {
        tiny_http::Server::http_bounded(
            ("127.0.0.1", 0),
            tiny_http::ServerLimits {
                max_connections: connections,
                max_queued_requests: queue,
                max_header_bytes: 256,
                max_body_wire_bytes: 8 * 1024 * 1024,
                connection_lifetime: Duration::from_millis(lifetime),
            },
        )
        .expect("HTTP resource test")
    }
    fn peer(server: &tiny_http::Server) -> TcpStream {
        let peer = TcpStream::connect(server.server_addr().to_ip().expect("HTTP resource test"))
            .expect("HTTP resource test");
        peer.set_read_timeout(Some(Duration::from_millis(800)))
            .expect("HTTP resource test");
        peer
    }
    fn get(peer: &mut TcpStream, path: &str) {
        write!(peer, "GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").expect("HTTP resource test");
    }
    fn closed(peer: &mut TcpStream) {
        let mut bytes = Vec::new();
        peer.read_to_end(&mut bytes).expect("HTTP resource test");
    }
    fn upload(length: usize) -> (tiny_http::Server, TcpStream, Request) {
        let server = tiny_http::Server::http(("127.0.0.1", 0)).expect("HTTP resource test");
        let mut peer = peer(&server);
        write!(peer, "POST / HTTP/1.1\r\nContent-Length: {length}\r\n\r\n")
            .expect("HTTP resource test");
        let request = server
            .recv_timeout(Duration::from_millis(500))
            .expect("HTTP resource test")
            .expect("HTTP resource test");
        (server, peer, request)
    }
    #[test]
    fn huge_content_length_abort_has_fixed_memory_and_does_not_panic() {
        let (_server, mut peer, request) = upload(usize::MAX);
        let start = Instant::now();
        request.abort();
        assert!(start.elapsed() < Duration::from_millis(100));
        closed(&mut peer);
    }
    #[test]
    fn failed_response_spawn_aborts_before_closure_body_drop() {
        let (_server, mut peer, request) = upload(2048);
        let start = Instant::now();
        let result = spawn_response(
            thread::Builder::new().stack_size(1usize << 62),
            request,
            tiny_http::Response::from_data(Vec::new()).with_status_code(401),
            (),
        );
        assert!(result.is_err());
        assert!(start.elapsed() < Duration::from_millis(100));
        closed(&mut peer);
    }
    #[test]
    fn ready_broken_pipe_is_nonfatal() {
        struct Closed;
        impl Write for Closed {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        write_ready(&mut Closed, "ready");
    }
    #[test]
    fn connection_permits_cover_queued_requests_and_release_after_abort() {
        let server = bounded(2, 2, 500);
        let mut first = peer(&server);
        get(&mut first, "/first");
        let first_request = server
            .recv_timeout(Duration::from_millis(300))
            .expect("HTTP resource test")
            .expect("HTTP resource test");
        let mut second = peer(&server);
        get(&mut second, "/second");
        let second_request = server
            .recv_timeout(Duration::from_millis(300))
            .expect("HTTP resource test")
            .expect("HTTP resource test");
        let mut third = peer(&server);
        get(&mut third, "/third");
        assert!(
            server
                .recv_timeout(Duration::from_millis(40))
                .expect("HTTP resource test")
                .is_none()
        );
        first_request.abort();
        closed(&mut first);
        let third_request = server
            .recv_timeout(Duration::from_millis(300))
            .expect("HTTP resource test")
            .expect("HTTP resource test");
        assert_eq!(third_request.url(), "/third");
        third_request.abort();
        second_request.abort();
    }
    #[test]
    fn full_request_queue_aborts_extra_socket_and_recovers_permit() {
        let server = bounded(3, 1, 500);
        let mut first = peer(&server);
        get(&mut first, "/first");
        thread::sleep(Duration::from_millis(40));
        let mut extra = peer(&server);
        get(&mut extra, "/extra");
        closed(&mut extra);
        let first_request = server
            .recv_timeout(Duration::from_millis(100))
            .expect("HTTP resource test")
            .expect("HTTP resource test");
        assert_eq!(first_request.url(), "/first");
        first_request.abort();
        let mut next = peer(&server);
        get(&mut next, "/next");
        let request = server
            .recv_timeout(Duration::from_millis(200))
            .expect("HTTP resource test")
            .expect("HTTP resource test");
        assert_eq!(request.url(), "/next");
        request.abort();
    }
    #[test]
    fn absolute_deadline_covers_trickling_headers_and_small_body_parser() {
        let server = bounded(1, 1, 100);
        let mut slow = peer(&server);
        let mut writer = slow.try_clone().expect("HTTP resource test");
        let trickle = thread::spawn(move || {
            for _ in 0..15 {
                if writer.write_all(b"G").is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
        });
        let start = Instant::now();
        closed(&mut slow);
        assert!(start.elapsed() < Duration::from_millis(250));
        trickle.join().expect("HTTP resource test");
        let mut upload = peer(&server);
        write!(upload, "POST / HTTP/1.1\r\nContent-Length: 1\r\n\r\n").expect("HTTP resource test");
        closed(&mut upload);
        assert!(server.try_recv().expect("HTTP resource test").is_none());
        let mut next = peer(&server);
        get(&mut next, "/next");
        server
            .recv_timeout(Duration::from_millis(200))
            .expect("HTTP resource test")
            .expect("HTTP resource test")
            .abort();
    }
    #[test]
    fn header_limit_http_version_and_single_exchange_release_connections() {
        let server = bounded(1, 1, 400);
        let mut oversized = peer(&server);
        oversized
            .write_all(&[b'A'; 300])
            .expect("HTTP resource test");
        closed(&mut oversized);
        assert!(server.try_recv().expect("HTTP resource test").is_none());
        let mut newer = peer(&server);
        newer
            .write_all(b"GET / HTTP/2.0\r\n\r\n")
            .expect("HTTP resource test");
        let mut response = String::new();
        newer
            .read_to_string(&mut response)
            .expect("HTTP resource test");
        assert!(response.contains("505"));
        let mut pipeline = peer(&server);
        pipeline
            .write_all(b"GET /one HTTP/1.1\r\n\r\nGET /two HTTP/1.1\r\n\r\n")
            .expect("HTTP resource test");
        let request = server
            .recv_timeout(Duration::from_millis(200))
            .expect("HTTP resource test")
            .expect("HTTP resource test");
        assert_eq!(request.url(), "/one");
        request
            .respond_and_close(tiny_http::Response::empty(200))
            .expect("HTTP resource test");
        closed(&mut pipeline);
        assert!(server.try_recv().expect("HTTP resource test").is_none());
    }
    #[test]
    fn request_drop_and_slow_response_write_release_permits() {
        let server = bounded(1, 1, 100);
        let mut upload = peer(&server);
        upload
            .write_all(b"POST / HTTP/1.1\r\nContent-Length: 2048\r\n\r\n")
            .expect("HTTP resource test");
        let request = server
            .recv_timeout(Duration::from_millis(200))
            .expect("HTTP resource test")
            .expect("HTTP resource test");
        let start = Instant::now();
        drop(request);
        assert!(start.elapsed() < Duration::from_millis(100));
        closed(&mut upload);
        let mut slow_reader = peer(&server);
        get(&mut slow_reader, "/large");
        let request = server
            .recv_timeout(Duration::from_millis(200))
            .expect("HTTP resource test")
            .expect("HTTP resource test");
        let start = Instant::now();
        let worker = spawn_response(
            thread::Builder::new(),
            request,
            tiny_http::Response::from_data(vec![b'x'; 8 * 1024 * 1024]),
            (),
        )
        .expect("HTTP resource test");
        worker.join().expect("HTTP resource test");
        assert!(start.elapsed() < Duration::from_millis(400));
        let mut next = peer(&server);
        get(&mut next, "/after-write");
        server
            .recv_timeout(Duration::from_millis(200))
            .expect("HTTP resource test")
            .expect("HTTP resource test")
            .abort();
    }

    #[test]
    fn server_drop_aborts_parser_and_queued_huge_body_without_waiting() {
        let server = bounded(2, 2, 1000);
        let mut parser = peer(&server);
        parser.write_all(b"GET ").expect("HTTP resource test");
        let mut queued = peer(&server);
        write!(
            queued,
            "POST / HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            usize::MAX
        )
        .expect("HTTP resource test");
        thread::sleep(Duration::from_millis(40));
        let start = Instant::now();
        drop(server);
        assert!(start.elapsed() < Duration::from_millis(100));
        closed(&mut parser);
        closed(&mut queued);
    }
}

#[cfg(test)]
mod chunked_metadata_tests {
    use std::{
        io::{Read, Write},
        net::TcpStream,
        time::Duration,
    };
    fn request(
        wire_limit: usize,
        body: &[u8],
    ) -> (tiny_http::Server, TcpStream, tiny_http::Request) {
        let server = tiny_http::Server::http_bounded(
            ("127.0.0.1", 0),
            tiny_http::ServerLimits {
                max_connections: 1,
                max_queued_requests: 1,
                max_header_bytes: 256,
                max_body_wire_bytes: wire_limit,
                connection_lifetime: Duration::from_secs(1),
            },
        )
        .expect("bounded server");
        let mut peer =
            TcpStream::connect(server.server_addr().to_ip().expect("address")).expect("peer");
        peer.write_all(b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n")
            .expect("headers");
        peer.write_all(body).expect("encoded body");
        let request = server
            .recv_timeout(Duration::from_millis(500))
            .expect("receive")
            .expect("request");
        (server, peer, request)
    }
    #[test]
    fn encoded_size_line_and_extension_stop_before_decoded_body_limit() {
        let mut size_line = vec![b'0'; 4096];
        size_line.extend_from_slice(b"\r\n\r\n");
        let mut extension = b"1;".to_vec();
        extension.extend_from_slice(&[b'a'; 4096]);
        extension.extend_from_slice(b"\r\nx\r\n0\r\n\r\n");
        for body in [&size_line, &extension] {
            let (_server, _peer, mut request) = request(1024, body);
            let mut decoded = String::new();
            assert!(
                request
                    .as_reader()
                    .take(65537)
                    .read_to_string(&mut decoded)
                    .is_err()
            );
            assert!(decoded.is_empty());
            request.abort();
        }
    }
    #[test]
    fn valid_fragmented_chunked_body_retains_support() {
        let mut encoded = Vec::new();
        for _ in 0..65536 {
            encoded.extend_from_slice(b"1\r\nx\r\n");
        }
        encoded.extend_from_slice(b"0\r\n\r\n");
        let (_server, _peer, mut request) = request(1024 * 1024, &encoded);
        let mut decoded = Vec::new();
        request
            .as_reader()
            .take(65537)
            .read_to_end(&mut decoded)
            .expect("valid chunked body");
        assert_eq!(decoded.len(), 65536);
        request.abort();
    }
}
