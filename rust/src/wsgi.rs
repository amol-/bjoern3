use std::cell::RefCell;
use std::collections::VecDeque;

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyIterator, PyList, PyString};

use crate::request::{header_value, should_keep_alive, ParsedRequest};

#[derive(Default)]
pub(crate) struct ResponseState {
    pub(crate) status: Option<String>,
    pub(crate) headers: Vec<(String, String)>,
}

#[pyclass]
pub(crate) struct StartResponse {
    state: RefCell<ResponseState>,
}

#[pymethods]
impl StartResponse {
    #[__call__]
    fn call(&self, status: &str, headers: &PyAny, _exc_info: Option<&PyAny>) -> PyResult<()> {
        let parsed_headers: Vec<(String, String)> = headers.extract()?;

        let mut state = self.state.borrow_mut();
        state.status = Some(status.to_string());
        state.headers = parsed_headers;
        Ok(())
    }
}

pub(crate) struct ResponseStream {
    pub(crate) pending: VecDeque<Vec<u8>>,
    pub(crate) iterator: Option<Py<PyAny>>,
    pub(crate) iterable: Option<Py<PyAny>>,
    pub(crate) chunked: bool,
    pub(crate) keep_alive: bool,
    pub(crate) finished: bool,
}

// Intent: build a WSGI environ mapping for the Python app.
// Result: each request includes headers, socket metadata, and request body input.
pub(crate) fn build_environ(
    py: Python<'_>,
    request: &ParsedRequest,
    local_addr: Option<std::net::SocketAddr>,
    remote_addr: Option<std::net::SocketAddr>,
) -> PyResult<Py<PyAny>> {
    let path = request.path.as_str();
    let mut parts = path.splitn(2, '?');
    let path_info = parts.next().unwrap_or("");
    let query_string = parts.next().unwrap_or("");

    let environ = PyDict::new(py);
    environ.set_item("REQUEST_METHOD", request.method.as_str())?;
    environ.set_item("PATH_INFO", path_info)?;
    environ.set_item("QUERY_STRING", query_string)?;
    environ.set_item("SERVER_PROTOCOL", format!("HTTP/1.{}", request.version))?;
    environ.set_item("wsgi.version", (1, 0))?;
    environ.set_item("wsgi.url_scheme", "http")?;
    environ.set_item("wsgi.multithread", false)?;
    environ.set_item("wsgi.multiprocess", false)?;
    environ.set_item("wsgi.run_once", false)?;

    let host_header = header_value(&request.headers, "Host");
    if let Some(host) = host_header {
        let mut host_parts = host.splitn(2, ':');
        let name = host_parts.next().unwrap_or("");
        let port = host_parts.next().unwrap_or("80");
        environ.set_item("SERVER_NAME", name)?;
        environ.set_item("SERVER_PORT", port)?;
    } else if let Some(addr) = local_addr {
        environ.set_item("SERVER_NAME", addr.ip().to_string())?;
        environ.set_item("SERVER_PORT", addr.port().to_string())?;
    } else {
        environ.set_item("SERVER_NAME", "unix")?;
        environ.set_item("SERVER_PORT", "0")?;
    }

    if let Some(addr) = remote_addr {
        environ.set_item("REMOTE_ADDR", addr.ip().to_string())?;
        environ.set_item("REMOTE_PORT", addr.port().to_string())?;
    }

    let io_module = py.import("io")?;
    let input = io_module.call_method1("BytesIO", (PyBytes::new(py, &request.body),))?;
    environ.set_item("wsgi.input", input)?;

    let sys_module = py.import("sys")?;
    environ.set_item("wsgi.errors", sys_module.getattr("stderr")?)?;

    if let Some(content_length) = header_value(&request.headers, "Content-Length") {
        environ.set_item("CONTENT_LENGTH", content_length)?;
    }
    if let Some(content_type) = header_value(&request.headers, "Content-Type") {
        environ.set_item("CONTENT_TYPE", content_type)?;
    }

    for (name, value) in request.headers.iter() {
        if name.eq_ignore_ascii_case("Content-Length") || name.eq_ignore_ascii_case("Content-Type")
        {
            continue;
        }
        let header_name = format!("HTTP_{}", name.to_uppercase().replace('-', "_"));
        environ.set_item(header_name, value)?;
    }

    Ok(environ.into())
}

fn response_body_iter(py: Python<'_>, response: &PyAny) -> PyResult<PyObject> {
    if response.is_instance_of::<PyBytes>()? || response.is_instance_of::<PyString>()? {
        let list = PyList::new(py, &[response])?;
        Ok(list.into())
    } else {
        Ok(response.to_object(py))
    }
}

fn next_non_empty_chunk(py: Python<'_>, iterator: &PyAny) -> PyResult<Option<Vec<u8>>> {
    let mut iter = PyIterator::from_object(py, iterator)?;
    for item in iter.by_ref() {
        let item = item?;
        if let Ok(bytes) = item.downcast::<PyBytes>() {
            if bytes.as_bytes().is_empty() {
                continue;
            }
            return Ok(Some(bytes.as_bytes().to_vec()));
        }
        if let Ok(text) = item.extract::<String>() {
            if text.is_empty() {
                continue;
            }
            return Ok(Some(text.as_bytes().to_vec()));
        }
        return Err(PyValueError::new_err(
            "response body items must be bytes or string",
        ));
    }
    Ok(None)
}

fn wrap_chunk(chunk: &[u8]) -> Vec<u8> {
    let size = format!("{:X}\r\n", chunk.len());
    let mut wrapped = Vec::with_capacity(size.len() + chunk.len() + 2);
    wrapped.extend_from_slice(size.as_bytes());
    wrapped.extend_from_slice(chunk);
    wrapped.extend_from_slice(b"\r\n");
    wrapped
}

fn build_headers(
    status: &str,
    headers: &[(String, String)],
    version: u8,
    keep_alive: bool,
    chunked: bool,
    content_length: Option<usize>,
) -> Vec<u8> {
    let mut response = Vec::new();
    response.extend_from_slice(format!("HTTP/1.{} {}\r\n", version, status).as_bytes());

    for (name, value) in headers {
        if name.eq_ignore_ascii_case("connection") {
            continue;
        }
        if name.eq_ignore_ascii_case("transfer-encoding") && chunked {
            continue;
        }
        if name.eq_ignore_ascii_case("content-length") && content_length.is_some() {
            continue;
        }
        response.extend_from_slice(format!("{}: {}\r\n", name, value).as_bytes());
    }

    if keep_alive {
        response.extend_from_slice(b"Connection: Keep-Alive\r\n");
    } else {
        response.extend_from_slice(b"Connection: close\r\n");
    }

    if chunked {
        response.extend_from_slice(b"Transfer-Encoding: chunked\r\n");
    } else if let Some(length) = content_length {
        response.extend_from_slice(format!("Content-Length: {}\r\n", length).as_bytes());
    }

    response.extend_from_slice(b"\r\n");
    response
}

fn close_iterable(py: Python<'_>, iterable: &Py<PyAny>) {
    let iterable = iterable.as_ref(py);
    if let Ok(close) = iterable.getattr("close") {
        let _ = close.call0();
    }
}

// Intent: prepare a streaming HTTP response from the WSGI app.
// Result: the server receives headers and a response iterator for incremental writes.
pub(crate) fn prepare_response(
    py: Python<'_>,
    request: &ParsedRequest,
    wsgi_app: &Py<PyAny>,
    local_addr: Option<std::net::SocketAddr>,
    remote_addr: Option<std::net::SocketAddr>,
) -> PyResult<ResponseStream> {
    let environ = build_environ(py, request, local_addr, remote_addr)?;

    let start_response = Py::new(
        py,
        StartResponse {
            state: RefCell::new(ResponseState::default()),
        },
    )?;

    let response = wsgi_app.as_ref(py).call1((environ, start_response.clone_ref(py)))?;

    let mut first_chunk: Option<Vec<u8>> = None;
    let mut iterator: Option<Py<PyAny>> = None;
    let mut iterable: Option<Py<PyAny>> = None;

    if let Ok(list) = response.downcast::<PyList>() {
        if list.len() == 1 {
            let item = list.get_item(0)?;
            if let Ok(bytes) = item.downcast::<PyBytes>() {
                if !bytes.as_bytes().is_empty() {
                    first_chunk = Some(bytes.as_bytes().to_vec());
                }
            } else if let Ok(text) = item.extract::<String>() {
                if !text.is_empty() {
                    first_chunk = Some(text.as_bytes().to_vec());
                }
            } else {
                return Err(PyValueError::new_err(
                    "response body items must be bytes or string",
                ));
            }
        } else {
            let iter_obj = response_body_iter(py, response)?;
            iterable = Some(response.into_py(py));
            iterator = Some(iter_obj.into_py(py));
            first_chunk = next_non_empty_chunk(py, iterator.as_ref().unwrap().as_ref(py))?;
        }
    } else if response.is_instance_of::<PyBytes>()? || response.is_instance_of::<PyString>()? {
        if let Ok(bytes) = response.downcast::<PyBytes>() {
            if !bytes.as_bytes().is_empty() {
                first_chunk = Some(bytes.as_bytes().to_vec());
            }
        } else if let Ok(text) = response.extract::<String>() {
            if !text.is_empty() {
                first_chunk = Some(text.as_bytes().to_vec());
            }
        }
    } else {
        let iter_obj = response_body_iter(py, response)?;
        iterable = Some(response.into_py(py));
        iterator = Some(iter_obj.into_py(py));
        first_chunk = next_non_empty_chunk(py, iterator.as_ref().unwrap().as_ref(py))?;
    }

    let start_response_ref = start_response.borrow(py);
    let state = start_response_ref.state.borrow();
    let status_line = state
        .status
        .clone()
        .ok_or_else(|| PyValueError::new_err("start_response was not called"))?;

    let is_no_body = status_line.starts_with("204") || status_line.starts_with("304");

    let keep_alive_requested = should_keep_alive(request.version, &request.headers);
    let content_length_header = header_value(&state.headers, "Content-Length")
        .and_then(|value| value.parse::<usize>().ok());

    let response_length_unknown = content_length_header.is_none() && !is_no_body;
    let mut keep_alive = keep_alive_requested;
    let mut chunked = false;

    if keep_alive_requested {
        if response_length_unknown {
            if request.version == 1 {
                chunked = true;
                keep_alive = true;
            } else {
                keep_alive = false;
            }
        }
    } else {
        keep_alive = false;
    }

    let content_length = if chunked {
        None
    } else if let Some(length) = content_length_header {
        Some(length)
    } else if iterator.is_none() {
        Some(first_chunk.as_ref().map(|chunk| chunk.len()).unwrap_or(0))
    } else {
        None
    };

    if !chunked && response_length_unknown && keep_alive {
        keep_alive = false;
    }

    let headers = build_headers(
        &status_line,
        &state.headers,
        request.version,
        keep_alive,
        chunked,
        content_length,
    );

    let mut pending = VecDeque::new();
    pending.push_back(headers);

    if let Some(chunk) = first_chunk {
        if chunked {
            pending.push_back(wrap_chunk(&chunk));
        } else {
            pending.push_back(chunk);
        }
    }

    Ok(ResponseStream {
        pending,
        iterator,
        iterable,
        chunked,
        keep_alive,
        finished: iterator.is_none(),
    })
}

// Intent: advance the WSGI response iterator for streaming writes.
// Result: callers receive the next response chunk or the final chunk terminator.
pub(crate) fn next_chunk(py: Python<'_>, response: &mut ResponseStream) -> PyResult<Option<Vec<u8>>> {
    if response.finished {
        return Ok(None);
    }

    let iterator = match response.iterator.as_ref() {
        Some(iterator) => iterator,
        None => {
            response.finished = true;
            return Ok(None);
        }
    };

    let next = next_non_empty_chunk(py, iterator.as_ref(py))?;
    if let Some(chunk) = next {
        if response.chunked {
            return Ok(Some(wrap_chunk(&chunk)));
        }
        return Ok(Some(chunk));
    }

    response.finished = true;
    if let Some(iterable) = response.iterable.as_ref() {
        close_iterable(py, iterable);
    }

    if response.chunked {
        return Ok(Some(b"0\r\n\r\n".to_vec()));
    }

    Ok(None)
}
