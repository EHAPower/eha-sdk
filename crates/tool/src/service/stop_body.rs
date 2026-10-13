// Copyright The eha-sdk Contributors

//! 独立读取经授权 Stop 请求体的有界入口。
//!
//! Stop 必须在普通慢请求之后仍可取得请求体；这里复用 `http_body` 的解析 worker，
//! 但每个实例独占自己的 worker 和容量为八的收发队列。

use super::http_body::{BodyReader, ReadyRequest};
use serde_json::Value;
use std::sync::mpsc::TryRecvError;

pub(super) struct StopBodyReader {
    reader: BodyReader,
}

impl StopBodyReader {
    pub(super) fn new() -> Self {
        Self {
            reader: BodyReader::new(),
        }
    }

    #[allow(clippy::result_large_err)] // 准入失败时返回请求，以便立即响应。
    pub(super) fn submit_with_envelope(
        &self,
        request: tiny_http::Request,
        error_envelope: Option<Value>,
    ) -> Result<(), tiny_http::Request> {
        self.reader.submit_with_envelope(request, error_envelope)
    }

    pub(super) fn try_recv(&self) -> Result<ReadyRequest, TryRecvError> {
        self.reader.try_recv()
    }
}

/// Stop 路由只允许固定的 v2 形状；查询参数、空 ID 和额外路径段均不是 Stop。
pub(super) fn is_stop_path(path: &str) -> bool {
    if path.contains('?') {
        return false;
    }
    let segments: Vec<_> = path.split('/').collect();
    match segments.as_slice() {
        ["", "service", "v2", "bindings", binding, "stop"] => !binding.is_empty(),
        [
            "",
            "service",
            "v2",
            "bindings",
            binding,
            "trials",
            trial,
            "stop",
        ] => !binding.is_empty() && !trial.is_empty(),
        ["", "service", "v2", "groups", group, "stop"] => !group.is_empty(),
        ["", "service", "v2", "control-scopes", scope, "stop"] => !scope.is_empty(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{StopBodyReader, is_stop_path};
    use crate::service::http_body::BodyReader;
    use std::{io::Write, net::TcpStream, sync::mpsc::TryRecvError, thread, time::Duration};
    use tiny_http::Server;

    #[test]
    fn recognizes_only_exact_v2_stop_paths() {
        for path in [
            "/service/v2/bindings/binding-1/stop",
            "/service/v2/bindings/binding-1/trials/trial-1/stop",
            "/service/v2/groups/group-1/stop",
            "/service/v2/control-scopes/stop-1/stop",
        ] {
            assert!(is_stop_path(path), "{path}");
        }
        for path in [
            "/service/v2/bindings//stop",
            "/service/v2/bindings/binding-1/stop/",
            "/service/v2/bindings/binding-1/stop?retry=1",
            "/service/v2/bindings/binding-1/trials//stop",
            "/service/v2/groups/group-1/stop/again",
            "/service/v2/groups/group-1/status",
        ] {
            assert!(!is_stop_path(path), "{path}");
        }
    }

    #[test]
    fn large_group_stop_body_completes_while_ordinary_reader_waits()
    -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let server = Server::http(("127.0.0.1", 0))?;
        let address = server.server_addr().to_ip().ok_or("no IP socket")?;
        let port = address.port();
        let mut slow = TcpStream::connect(address)?;
        slow.write_all(format!("POST /service/v2/bindings HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: 2048\r\nConnection: close\r\n\r\n{{\"api_version\":2").as_bytes())?;

        let ordinary = BodyReader::new();
        let slow_request = server
            .recv_timeout(Duration::from_secs(1))?
            .ok_or("slow ordinary request was not accepted")?;
        ordinary
            .submit(slow_request)
            .map_err(|_| "ordinary reader unexpectedly full")?;

        let padding = "x".repeat(2048);
        let stop_body = format!(r#"{{"api_version":2,"padding":"{padding}"}}"#);
        let mut stop = TcpStream::connect(address)?;
        stop.write_all(
            format!(
                "POST /service/v2/groups/group-1/stop HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{stop_body}",
                stop_body.len(),
            )
            .as_bytes(),
        )?;
        let stop_request = server
            .recv_timeout(Duration::from_secs(1))?
            .ok_or("stop request was not accepted")?;
        let stop_reader = StopBodyReader::new();
        stop_reader
            .submit_with_envelope(stop_request, None)
            .map_err(|_| "stop reader unexpectedly full")?;

        let mut owner = crate::service::Sessions::new();
        let mut ticks = 0;
        let ready = loop {
            ticks += 1;
            owner.tick();
            match stop_reader.try_recv() {
                Ok(ready) => break ready,
                Err(TryRecvError::Empty) => thread::yield_now(),
                Err(TryRecvError::Disconnected) => return Err("stop reader disconnected".into()),
            }
            if ticks == 10_000 {
                return Err("stop reader did not complete".into());
            }
        };
        assert!(ticks >= 1);
        assert!(ready.body.is_ok());
        assert!(matches!(ordinary.try_recv(), Err(TryRecvError::Empty)));
        drop(slow);
        Ok(())
    }
}
