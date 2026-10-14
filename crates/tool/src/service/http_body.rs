// Copyright The eha-sdk Contributors
//! Bounded HTTP JSON body reader for the WebUI.
//!
//! A client can send headers then indefinitely delay its body.  Reading that body from the
//! session loop would prevent trial deadlines and Stop confirmation from being polled.  This
//! worker owns at most one such reader and both sides of its hand-off are bounded.

use super::{parse_json_body, respond_json};
use serde_json::{Value, json};
use std::{
    sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
    thread::{self, JoinHandle},
};

const QUEUE_CAPACITY: usize = 8;

struct BodyJob {
    request: tiny_http::Request,
    error_envelope: Option<Value>,
}

pub(super) struct ReadyRequest {
    pub(super) request: tiny_http::Request,
    pub(super) body: Result<crate::RawFields, String>,
    error_envelope: Option<Value>,
}

pub(super) struct BodyReader {
    jobs: SyncSender<BodyJob>,
    completed: Receiver<ReadyRequest>,
    _worker: JoinHandle<()>,
}

impl BodyReader {
    pub(super) fn new() -> Self {
        let (jobs, receiver) = mpsc::sync_channel::<BodyJob>(QUEUE_CAPACITY);
        let (completed, ready) = mpsc::sync_channel::<ReadyRequest>(QUEUE_CAPACITY);
        let worker = thread::Builder::new()
            .name("eha-tool-webui-body-reader".into())
            .spawn(move || read_bodies(receiver, completed))
            .expect("start WebUI body reader");
        Self {
            jobs,
            completed: ready,
            _worker: worker,
        }
    }

    /// Keep the session thread non-blocking when readers are already occupied.
    #[allow(clippy::result_large_err)] // Returning ownership lets the session loop send 503.
    #[cfg(test)]
    pub(super) fn submit(&self, request: tiny_http::Request) -> Result<(), tiny_http::Request> {
        self.submit_with_envelope(request, None)
    }

    #[allow(clippy::result_large_err)]
    pub(super) fn submit_with_envelope(
        &self,
        request: tiny_http::Request,
        error_envelope: Option<Value>,
    ) -> Result<(), tiny_http::Request> {
        match self.jobs.try_send(BodyJob {
            request,
            error_envelope,
        }) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(job) | TrySendError::Disconnected(job)) => Err(job.request),
        }
    }

    pub(super) fn try_recv(&self) -> Result<ReadyRequest, TryRecvError> {
        self.completed.try_recv()
    }
}

fn read_bodies(receiver: Receiver<BodyJob>, completed: SyncSender<ReadyRequest>) {
    while let Ok(BodyJob {
        mut request,
        error_envelope,
    }) = receiver.recv()
    {
        let body = parse_json_body(&mut request);
        let ready = ReadyRequest {
            request,
            body,
            error_envelope,
        };
        match completed.try_send(ready) {
            Ok(()) => {}
            Err(TrySendError::Full(ready)) => {
                let _ = respond_json(ready.request, 503, queue_error(ready.error_envelope));
            }
            Err(TrySendError::Disconnected(_)) => return,
        }
    }
}

pub(super) fn queue_error(envelope: Option<Value>) -> Value {
    let Some(mut value) = envelope else {
        return json!({"ok":false,"message":"WebUI 请求队列已满；请稍后重试。"});
    };
    value["code"] = json!("busy");
    value["message"] = json!("服务请求队列已满；请稍后重试。");
    value
}

#[cfg(test)]
mod tests {
    use super::BodyReader;
    use crate::service::{
        CanScanView, OdriveView, Sessions, handle_received_request, json_body_is_prebuffered,
    };
    use std::{
        io::{Read, Write},
        net::TcpStream,
        sync::mpsc::TryRecvError,
        thread,
        time::Duration,
    };
    use tiny_http::Server;

    #[test]
    fn slow_authenticated_v2_body_leaves_describe_responsive()
    -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let server = Server::http(("127.0.0.1", 0))?;
        let address = server.server_addr().to_ip().ok_or("no IP socket")?;
        let port = address.port();
        let mut slow = TcpStream::connect(address)?;
        slow.write_all(format!("POST /service/v2/bindings HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer test-token\r\nContent-Type: application/json\r\nContent-Length: 2048\r\nConnection: close\r\n\r\n{{\"api_version\":2").as_bytes())?;
        let mut workbench = crate::service::prepare_workbench(
            None,
            Some(crate::service::integration::IntegrationState::for_test()),
            crate::service::RouteMode::ServiceV2,
        );
        let mut odrive = OdriveView::new(None);
        let mut scan = CanScanView::new();
        let reader = BodyReader::new();
        let request = server
            .recv_timeout(Duration::from_secs(1))?
            .ok_or("slow POST not accepted")?;
        handle_received_request(
            request,
            port,
            &mut workbench,
            &mut odrive,
            &mut scan,
            &reader,
            &crate::service::stop_body::StopBodyReader::new(),
        )?;
        let client = thread::spawn(move || -> std::io::Result<String> {
            let mut stream = TcpStream::connect(address)?;
            stream.set_read_timeout(Some(Duration::from_secs(1)))?;
            stream.write_all(format!("GET /service/v2/describe HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer test-token\r\nConnection: close\r\n\r\n").as_bytes())?;
            let mut response = String::new();
            stream.read_to_string(&mut response)?;
            Ok(response)
        });
        workbench.tick();
        let request = server
            .recv_timeout(Duration::from_secs(1))?
            .ok_or("GET not accepted")?;
        handle_received_request(
            request,
            port,
            &mut workbench,
            &mut odrive,
            &mut scan,
            &reader,
            &crate::service::stop_body::StopBodyReader::new(),
        )?;
        let response = client.join().map_err(|_| "client panic")??;
        assert!(response.starts_with("HTTP/1.1 200"));
        assert!(response.contains("eha-tool.service.v2"));
        assert!(matches!(reader.try_recv(), Err(TryRecvError::Empty)));
        let busy = super::queue_error(Some(crate::service::v2::envelope("i-test")));
        assert_eq!(busy["api_version"], 2);
        assert_eq!(busy["code"], "busy");
        drop(slow);
        Ok(())
    }

    #[test]
    fn slow_post_body_leaves_ticks_and_get_responsive()
    -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let server = Server::http(("127.0.0.1", 0))?;
        let address = server
            .server_addr()
            .to_ip()
            .ok_or("test server did not use an IP socket")?;
        let port = address.port();
        let mut slow = TcpStream::connect(address)?;
        slow.write_all(
            format!(
                "POST /api/action HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: http://127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: 2048\r\nConnection: close\r\n\r\n{{\"transport\":\"usb\""
            )
            .as_bytes(),
        )?;

        let reader = BodyReader::new();
        let first = server
            .recv_timeout(Duration::from_secs(1))?
            .ok_or("slow POST was not accepted")?;
        reader
            .submit(first)
            .map_err(|_| "body reader unexpectedly full")?;

        let stop_body = r#"{"transport":"usb","action":"stop"}"#;
        let stop = thread::spawn(move || -> Result<String, std::io::Error> {
            let mut stream = TcpStream::connect(address)?;
            stream.write_all(
                format!(
                    "POST /api/action HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: http://127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{stop_body}",
                    stop_body.len(),
                )
                .as_bytes(),
            )?;
            stream.set_read_timeout(Some(Duration::from_secs(1)))?;
            let mut response = String::new();
            stream.read_to_string(&mut response)?;
            Ok(response)
        });
        let mut sessions = Sessions::new();
        let mut odrive = OdriveView::new(None);
        let mut scan = CanScanView::new();
        let mut ticks = 0;
        let stop_request = loop {
            ticks += 1;
            sessions.tick();
            if let Some(request) = server.recv_timeout(Duration::from_millis(10))? {
                break request;
            }
        };
        assert!(json_body_is_prebuffered(&stop_request));
        handle_received_request(
            stop_request,
            port,
            &mut sessions,
            &mut odrive,
            &mut scan,
            &reader,
            &crate::service::stop_body::StopBodyReader::new(),
        )?;
        let stop = stop.join().map_err(|_| "Stop client panicked")??;
        assert!(stop.starts_with("HTTP/1.1 409"));

        let get = thread::spawn(move || -> Result<String, std::io::Error> {
            let mut stream = TcpStream::connect(address)?;
            stream
                .write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1:1\r\nConnection: close\r\n\r\n")?;
            stream.set_read_timeout(Some(Duration::from_secs(1)))?;
            let mut response = String::new();
            stream.read_to_string(&mut response)?;
            Ok(response)
        });
        let get_request = loop {
            ticks += 1;
            sessions.tick();
            if let Some(request) = server.recv_timeout(Duration::from_millis(10))? {
                break request;
            }
        };
        handle_received_request(
            get_request,
            port,
            &mut sessions,
            &mut odrive,
            &mut scan,
            &reader,
            &crate::service::stop_body::StopBodyReader::new(),
        )?;
        let response = get.join().map_err(|_| "GET client panicked")??;
        assert!(response.starts_with("HTTP/1.1 200"));
        assert!(ticks >= 1);
        assert!(matches!(reader.try_recv(), Err(TryRecvError::Empty)));
        drop(slow);
        Ok(())
    }
}
