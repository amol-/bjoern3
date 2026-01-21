use std::collections::VecDeque;
use std::io::{Read, Write};
use std::os::unix::io::FromRawFd;
use std::os::unix::net::UnixListener as StdUnixListener;

use libc::{dup, getsockname, sockaddr_storage, socklen_t, AF_UNIX};
use mio::net::{TcpListener as MioTcpListener, TcpStream as MioTcpStream};
use mio::{Events, Interest, Poll, Token, Waker};
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

use crate::request::read_next_request;
use crate::wsgi::{next_chunk, prepare_response, ResponseStream};

const LISTENER: Token = Token(0);
const WAKER: Token = Token(1);
const FIRST_CONN: usize = 2;

enum ConnectionStream {
    Tcp(MioTcpStream),
    Unix(mio::net::UnixStream),
}

impl Read for ConnectionStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            ConnectionStream::Tcp(stream) => stream.read(buf),
            ConnectionStream::Unix(stream) => stream.read(buf),
        }
    }
}

impl Write for ConnectionStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            ConnectionStream::Tcp(stream) => stream.write(buf),
            ConnectionStream::Unix(stream) => stream.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct Outgoing {
    data: Vec<u8>,
    offset: usize,
}

struct Connection {
    stream: ConnectionStream,
    buffer: Vec<u8>,
    outgoing: VecDeque<Outgoing>,
    response: Option<ResponseStream>,
    close_after_write: bool,
    local_addr: Option<std::net::SocketAddr>,
    remote_addr: Option<std::net::SocketAddr>,
}

fn socket_family(fd: i32) -> std::io::Result<i32> {
    let mut storage: sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of::<sockaddr_storage>() as socklen_t;
    let result = unsafe { getsockname(fd, &mut storage as *mut _ as *mut _, &mut length) };
    if result < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(storage.ss_family as i32)
}

fn register_connection(poll: &Poll, token: Token, stream: &mut ConnectionStream) -> std::io::Result<()> {
    match stream {
        ConnectionStream::Tcp(s) => poll.registry().register(s, token, Interest::READABLE),
        ConnectionStream::Unix(s) => poll.registry().register(s, token, Interest::READABLE),
    }
}

fn reregister_connection(
    poll: &Poll,
    token: Token,
    stream: &mut ConnectionStream,
    writable: bool,
) -> std::io::Result<()> {
    let interest = if writable {
        Interest::READABLE | Interest::WRITABLE
    } else {
        Interest::READABLE
    };
    match stream {
        ConnectionStream::Tcp(s) => poll.registry().reregister(s, token, interest),
        ConnectionStream::Unix(s) => poll.registry().reregister(s, token, interest),
    }
}

fn accept_tcp(
    poll: &Poll,
    listener: &mut MioTcpListener,
    connections: &mut Vec<Option<Connection>>,
) -> std::io::Result<()> {
    loop {
        match listener.accept() {
            Ok((stream, _addr)) => {
                let local_addr = stream.local_addr().ok();
                let remote_addr = stream.peer_addr().ok();
                let token = Token(connections.len() + FIRST_CONN);
                let mut connection = Connection {
                    stream: ConnectionStream::Tcp(stream),
                    buffer: Vec::new(),
                    outgoing: VecDeque::new(),
                    response: None,
                    close_after_write: false,
                    local_addr,
                    remote_addr,
                };
                register_connection(poll, token, &mut connection.stream)?;
                connections.push(Some(connection));
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

fn accept_unix(
    poll: &Poll,
    listener: &mut mio::net::UnixListener,
    connections: &mut Vec<Option<Connection>>,
) -> std::io::Result<()> {
    loop {
        match listener.accept() {
            Ok((stream, _addr)) => {
                let token = Token(connections.len() + FIRST_CONN);
                let mut connection = Connection {
                    stream: ConnectionStream::Unix(stream),
                    buffer: Vec::new(),
                    outgoing: VecDeque::new(),
                    response: None,
                    close_after_write: false,
                    local_addr: None,
                    remote_addr: None,
                };
                register_connection(poll, token, &mut connection.stream)?;
                connections.push(Some(connection));
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

fn write_outgoing(connection: &mut Connection) -> std::io::Result<bool> {
    while let Some(front) = connection.outgoing.front_mut() {
        match connection.stream.write(&front.data[front.offset..]) {
            Ok(0) => break,
            Ok(written) => {
                front.offset += written;
                if front.offset >= front.data.len() {
                    connection.outgoing.pop_front();
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => return Ok(true),
            Err(err) => return Err(err),
        }
    }
    Ok(!connection.outgoing.is_empty())
}

fn fill_outgoing_from_response(connection: &mut Connection) -> PyResult<()> {
    if let Some(response) = connection.response.as_mut() {
        if let Some(chunk) = Python::with_gil(|py| next_chunk(py, response))? {
            connection.outgoing.push_back(Outgoing { data: chunk, offset: 0 });
        } else if response.finished {
            connection.close_after_write = !response.keep_alive;
            connection.response = None;
        }
    }
    Ok(())
}

fn handle_readable(
    connection: &mut Connection,
    wsgi_app: &Py<PyAny>,
) -> PyResult<()> {
    if connection.response.is_some() {
        return Ok(());
    }

    loop {
        let request = match read_next_request(&mut connection.stream, &mut connection.buffer) {
            Ok(Some(request)) => request,
            Ok(None) => break,
            Err(err) => return Err(PyValueError::new_err(format!("failed reading request: {err}"))),
        };

        let mut response = Python::with_gil(|py| {
            prepare_response(
                py,
                &request,
                wsgi_app,
                connection.local_addr,
                connection.remote_addr,
            )
        })?;

        while let Some(pending) = response.pending.pop_front() {
            connection.outgoing.push_back(Outgoing {
                data: pending,
                offset: 0,
            });
        }

        connection.close_after_write = !response.keep_alive && response.finished;
        connection.response = Some(response);
        break;
    }

    Ok(())
}

// Intent: run the server loop using mio as a libev-style event loop replacement.
// Result: bjoern exposes server_run to execute a non-blocking WSGI-capable HTTP/1.1 loop.
pub(crate) fn server_run(sock: &PyAny, wsgi_app: &PyAny) -> PyResult<()> {
    let fd: i32 = sock.call_method0("fileno")?.extract()?;
    let dup_fd = unsafe { dup(fd) };
    if dup_fd < 0 {
        return Err(PyValueError::new_err("failed duplicating socket"));
    }

    let family = socket_family(dup_fd)
        .map_err(|err| PyValueError::new_err(format!("socket family lookup failed: {err}")))?;

    let mut poll = Poll::new().map_err(|err| PyValueError::new_err(err.to_string()))?;
    let mut events = Events::with_capacity(1024);
    let waker = Waker::new(poll.registry(), WAKER)
        .map_err(|err| PyValueError::new_err(err.to_string()))?;
    let _ = waker.wake();

    let wsgi_app = wsgi_app.into_py(sock.py());
    let mut connections: Vec<Option<Connection>> = Vec::new();

    if family == AF_UNIX {
        let std_listener = unsafe { StdUnixListener::from_raw_fd(dup_fd) };
        std_listener
            .set_nonblocking(true)
            .map_err(|err| PyValueError::new_err(err.to_string()))?;
        let mut listener = mio::net::UnixListener::from_std(std_listener);
        poll.registry()
            .register(&mut listener, LISTENER, Interest::READABLE)
            .map_err(|err| PyValueError::new_err(err.to_string()))?;

        loop {
            poll.poll(&mut events, None)
                .map_err(|err| PyValueError::new_err(err.to_string()))?;
            for event in events.iter() {
                match event.token() {
                    LISTENER => {
                        accept_unix(&poll, &mut listener, &mut connections)
                            .map_err(|err| PyValueError::new_err(err.to_string()))?;
                    }
                    WAKER => {}
                    token => {
                        let index = token.0 - FIRST_CONN;
                        if let Some(Some(connection)) = connections.get_mut(index) {
                            if event.is_readable() {
                                handle_readable(connection, &wsgi_app)?;
                            }
                            if event.is_writable() {
                                let pending = write_outgoing(connection)
                                    .map_err(|err| PyValueError::new_err(err.to_string()))?;
                                if !pending {
                                    fill_outgoing_from_response(connection)?;
                                    if connection.close_after_write && connection.outgoing.is_empty() {
                                        connections[index] = None;
                                        continue;
                                    }
                                }
                            }
                            let pending = !connection.outgoing.is_empty() || connection.response.is_some();
                            reregister_connection(&poll, token, &mut connection.stream, pending)
                                .map_err(|err| PyValueError::new_err(err.to_string()))?;
                        }
                    }
                }
            }
        }
    } else {
        let std_listener = unsafe { std::net::TcpListener::from_raw_fd(dup_fd) };
        std_listener
            .set_nonblocking(true)
            .map_err(|err| PyValueError::new_err(err.to_string()))?;
        let mut listener = MioTcpListener::from_std(std_listener);
        poll.registry()
            .register(&mut listener, LISTENER, Interest::READABLE)
            .map_err(|err| PyValueError::new_err(err.to_string()))?;

        loop {
            poll.poll(&mut events, None)
                .map_err(|err| PyValueError::new_err(err.to_string()))?;
            for event in events.iter() {
                match event.token() {
                    LISTENER => {
                        accept_tcp(&poll, &mut listener, &mut connections)
                            .map_err(|err| PyValueError::new_err(err.to_string()))?;
                    }
                    WAKER => {}
                    token => {
                        let index = token.0 - FIRST_CONN;
                        if let Some(Some(connection)) = connections.get_mut(index) {
                            if event.is_readable() {
                                handle_readable(connection, &wsgi_app)?;
                            }
                            if event.is_writable() {
                                let pending = write_outgoing(connection)
                                    .map_err(|err| PyValueError::new_err(err.to_string()))?;
                                if !pending {
                                    fill_outgoing_from_response(connection)?;
                                    if connection.close_after_write && connection.outgoing.is_empty() {
                                        connections[index] = None;
                                        continue;
                                    }
                                }
                            }
                            let pending = !connection.outgoing.is_empty() || connection.response.is_some();
                            reregister_connection(&poll, token, &mut connection.stream, pending)
                                .map_err(|err| PyValueError::new_err(err.to_string()))?;
                        }
                    }
                }
            }
        }
    }
}
