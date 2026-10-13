//! SDK opt-in limits. Upstream Server constructors retain their original behavior.
use crate::util::{MessagesQueue, RefinedTcpStream};
use crate::{ClientConnection, Connection, Listener, Message, Server};
use std::error::Error;
use std::net::Shutdown;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};
#[derive(Clone, Copy, Debug)]
pub struct ServerLimits {
    pub max_connections: usize,
    pub max_queued_requests: usize,
    pub max_header_bytes: usize,
    /// Maximum encoded body bytes, including chunk-size lines and extensions.
    pub max_body_wire_bytes: usize,
    /// Absolute lifetime from accept; trickling bytes never renews this deadline.
    pub connection_lifetime: Duration,
}
impl ServerLimits {
    pub(crate) fn validate(&self) -> std::io::Result<()> {
        if self.max_connections == 0
            || self.max_queued_requests == 0
            || self.max_header_bytes == 0
            || self.max_body_wire_bytes == 0
            || self.connection_lifetime.is_zero()
            || Instant::now()
                .checked_add(self.connection_lifetime)
                .is_none()
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid HTTP limits",
            ));
        }
        Ok(())
    }
}
pub(crate) struct State {
    close: Arc<AtomicBool>,
    count: Mutex<usize>,
    available: Condvar,
    leases: Mutex<Vec<Weak<Lease>>>,
    limits: ServerLimits,
}
struct Permit(Arc<State>);
impl Drop for Permit {
    fn drop(&mut self) {
        let mut count = self.0.count.lock().unwrap();
        *count -= 1;
        self.0.available.notify_all();
    }
}
pub(crate) struct Lease {
    connection: Mutex<Connection>,
    pub(crate) closed: Arc<AtomicBool>,
    deadline: Instant,
    // This permit outlives the parser and follows all actual read/write owners.
    _permit: Permit,
}
impl Lease {
    fn abort(&self) {
        if !self.closed.swap(true, Ordering::AcqRel) {
            let _ = self.connection.lock().unwrap().shutdown(Shutdown::Both);
        }
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        self.abort();
    }
}
impl State {
    fn acquire(self: &Arc<Self>) -> Option<Permit> {
        let mut count = self.count.lock().unwrap();
        while *count >= self.limits.max_connections && !self.close.load(Ordering::Acquire) {
            count = self.available.wait(count).unwrap();
        }
        if self.close.load(Ordering::Acquire) {
            return None;
        }
        *count += 1;
        Some(Permit(self.clone()))
    }
    pub(crate) fn stop(&self) {
        {
            // Pair close with the same mutex as acquire to avoid a lost wakeup.
            let _count = self.count.lock().unwrap();
            self.close.store(true, Ordering::Release);
            self.available.notify_all();
        }
        for lease in self.leases.lock().unwrap().iter().filter_map(Weak::upgrade) {
            lease.abort();
        }
    }
    fn monitor(&self) {
        while !self.close.load(Ordering::Acquire) {
            let now = Instant::now();
            self.leases.lock().unwrap().retain(|weak| {
                if let Some(lease) = weak.upgrade() {
                    if now >= lease.deadline {
                        lease.abort();
                    }
                    true
                } else {
                    false
                }
            });
            // Unlike the owner-loop deadline, this task also covers blocked parsers.
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
pub(crate) fn start(
    listener: Listener,
    limits: ServerLimits,
) -> Result<Server, Box<dyn Error + Send + Sync>> {
    let listening_addr = listener.local_addr()?;
    match &listener {
        Listener::Tcp(listener) => listener.set_nonblocking(true)?,
        #[cfg(unix)]
        Listener::Unix(listener) => listener.set_nonblocking(true)?,
    }
    let close = Arc::new(AtomicBool::new(false));
    let state = Arc::new(State {
        close: close.clone(),
        count: Mutex::new(0),
        available: Condvar::new(),
        leases: Mutex::new(Vec::with_capacity(limits.max_connections)),
        limits,
    });
    let messages = MessagesQueue::bounded(limits.max_queued_requests);
    let monitor = state.clone();
    std::thread::Builder::new()
        .name("tiny-http-deadlines".into())
        .spawn(move || monitor.monitor())?;
    let accept_state = state.clone();
    let accept_messages = messages.clone();
    let result = std::thread::Builder::new()
        .name("tiny-http-bounded-accept".into())
        .spawn(move || {
            // Reserve before accept: not even parser tasks or accepted sockets can exceed the cap.
            while let Some(permit) = accept_state.acquire() {
                let (socket, _) = match listener.accept() {
                    Ok(value) => value,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    Err(error) => {
                        let _ = accept_messages.try_push(Message::Error(error));
                        break;
                    }
                };
                if accept_state.close.load(Ordering::Acquire) {
                    let _ = socket.shutdown(Shutdown::Both);
                    break;
                }
                let blocking = match &socket {
                    Connection::Tcp(socket) => socket.set_nonblocking(false),
                    #[cfg(unix)]
                    Connection::Unix(socket) => socket.set_nonblocking(false),
                };
                if blocking.is_err() {
                    let _ = socket.shutdown(Shutdown::Both);
                    continue;
                }
                let control = match socket.try_clone() {
                    Ok(value) => value,
                    Err(_) => continue,
                };
                let lease = Arc::new(Lease {
                    connection: Mutex::new(control),
                    closed: Arc::new(AtomicBool::new(false)),
                    deadline: Instant::now() + limits.connection_lifetime,
                    _permit: permit,
                });
                {
                    let mut leases = accept_state.leases.lock().unwrap();
                    if accept_state.close.load(Ordering::Acquire) {
                        lease.abort();
                        break;
                    }
                    leases.retain(|lease| lease.strong_count() != 0);
                    leases.push(Arc::downgrade(&lease));
                }
                let (read, write) = RefinedTcpStream::new(socket);
                let client =
                    ClientConnection::new(write.with_lease(lease.clone()), read.with_lease(lease))
                        .with_limits(limits.max_header_bytes, limits.max_body_wire_bytes);
                let messages = accept_messages.clone();
                // On spawn failure the unparsed client closes through its lease without reading body.
                let _ = std::thread::Builder::new()
                    .name("tiny-http-bounded-client".into())
                    .spawn(move || {
                        for request in client {
                            if let Err(Message::NewRequest(request)) =
                                messages.try_push(request.into())
                            {
                                request.abort();
                            }
                        }
                    });
            }
        });
    if let Err(error) = result {
        state.stop();
        return Err(error.into());
    }
    Ok(Server {
        close,
        messages,
        listening_addr,
        bounded: Some(state),
    })
}
