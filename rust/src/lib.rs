mod request;
mod server;
mod wsgi;

use pyo3::prelude::*;

// Intent: expose request line parsing for Python callers.
// Result: Python can parse HTTP request lines via the Rust implementation.
#[pyfunction]
fn parse_request_line(request_line: &[u8]) -> PyResult<(String, String, String)> {
    request::parse_request_line(request_line)
}

// Intent: expose the Rust server loop for Python callers.
// Result: Python can run the non-blocking HTTP server from the bjoern module.
#[pyfunction]
fn server_run(sock: &PyAny, wsgi_app: &PyAny) -> PyResult<()> {
    server::server_run(sock, wsgi_app)
}

// Intent: register the Rust-backed WSGI server module for Python use.
// Result: importing _bjoern exposes server_run and parse_request_line.
#[pymodule]
fn _bjoern(_py: Python<'_>, module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_function(wrap_pyfunction!(server_run, module)?)?;
    module.add_function(wrap_pyfunction!(parse_request_line, module)?)?;
    module.add("__doc__", "Rust-backed bjoern server core")?;
    Ok(())
}
