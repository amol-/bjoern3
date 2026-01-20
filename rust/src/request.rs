use std::io::Read;

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

#[derive(Clone)]
pub(crate) struct ParsedRequest {
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) version: u8,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Vec<u8>,
}

// Intent: parse a request line using httparse as the Rust replacement for http-parser.
// Result: callers receive a method, path, and HTTP version triple on valid inputs.
pub(crate) fn parse_request_line(request_line: &[u8]) -> PyResult<(String, String, String)> {
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut request = httparse::Request::new(&mut headers);

    let status = request
        .parse(request_line)
        .map_err(|err| PyValueError::new_err(format!("invalid request line: {err}")))?;

    if !status.is_complete() {
        return Err(PyValueError::new_err("incomplete request line"));
    }

    let method = request
        .method
        .ok_or_else(|| PyValueError::new_err("request line missing method"))?;
    let path = request
        .path
        .ok_or_else(|| PyValueError::new_err("request line missing path"))?;
    let version = request
        .version
        .map(|ver| format!("HTTP/1.{ver}"))
        .ok_or_else(|| PyValueError::new_err("request line missing HTTP version"))?;

    Ok((method.to_string(), path.to_string(), version))
}

fn read_more<T: Read>(stream: &mut T, buffer: &mut Vec<u8>) -> std::io::Result<usize> {
    let mut temp = [0_u8; 4096];
    match stream.read(&mut temp) {
        Ok(read) => {
            if read > 0 {
                buffer.extend_from_slice(&temp[..read]);
            }
            Ok(read)
        }
        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => Ok(0),
        Err(err) => Err(err),
    }
}

fn parse_headers(
    buffer: &[u8],
) -> Result<httparse::Status<(ParsedRequest, usize)>, httparse::Error> {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut request = httparse::Request::new(&mut headers);
    match request.parse(buffer)? {
        httparse::Status::Complete(size) => {
            let header_pairs = request
                .headers
                .iter()
                .map(|header| {
                    (
                        header.name.to_string(),
                        String::from_utf8_lossy(header.value).to_string(),
                    )
                })
                .collect();

            let parsed = ParsedRequest {
                method: request.method.unwrap_or("GET").to_string(),
                path: request.path.unwrap_or("/").to_string(),
                version: request.version.unwrap_or(1),
                headers: header_pairs,
                body: Vec::new(),
            };
            Ok(httparse::Status::Complete((parsed, size)))
        }
        httparse::Status::Partial => Ok(httparse::Status::Partial),
    }
}

fn header_value_impl(headers: &[(String, String)], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(header_name, _)| header_name.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.to_string())
}

fn read_chunked_body<T: Read>(
    stream: &mut T,
    buffer: &mut Vec<u8>,
    mut cursor: usize,
) -> Result<Option<(Vec<u8>, usize)>, std::io::Error> {
    let mut body = Vec::new();
    loop {
        while cursor + 2 > buffer.len()
            || !buffer[cursor..].windows(2).any(|window| window == b"\r\n")
        {
            if read_more(stream, buffer)? == 0 {
                return Ok(None);
            }
        }
        let line_end = buffer[cursor..]
            .windows(2)
            .position(|window| window == b"\r\n")
            .map(|idx| cursor + idx)
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "chunk line missing"))?;
        let line = String::from_utf8_lossy(&buffer[cursor..line_end]);
        let size_str = line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_str, 16).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid chunk size")
        })?;
        cursor = line_end + 2;
        if size == 0 {
            while buffer.len() < cursor + 2 {
                if read_more(stream, buffer)? == 0 {
                    return Ok(None);
                }
            }
            cursor += 2;
            break;
        }
        while buffer.len() < cursor + size + 2 {
            if read_more(stream, buffer)? == 0 {
                return Ok(None);
            }
        }
        body.extend_from_slice(&buffer[cursor..cursor + size]);
        cursor += size + 2;
    }
    Ok(Some((body, cursor)))
}

// Intent: read and decode a full HTTP request for keep-alive handling.
// Result: the caller receives the next parsed request or None until data completes.
pub(crate) fn read_next_request<T: Read>(
    stream: &mut T,
    buffer: &mut Vec<u8>,
) -> Result<Option<ParsedRequest>, std::io::Error> {
    if buffer.is_empty() {
        let read = read_more(stream, buffer)?;
        if read == 0 {
            return Ok(None);
        }
    }

    let (mut request, header_len) = match parse_headers(buffer) {
        Ok(httparse::Status::Complete(result)) => result,
        Ok(httparse::Status::Partial) => return Ok(None),
        Err(error) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid request: {error}"),
            ))
        }
    };

    let content_length = header_value_impl(&request.headers, "Content-Length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    let is_chunked = header_value_impl(&request.headers, "Transfer-Encoding")
        .map(|value| value.to_ascii_lowercase().contains("chunked"))
        .unwrap_or(false);

    let (body, consumed) = if is_chunked {
        match read_chunked_body(stream, buffer, header_len)? {
            Some(result) => result,
            None => return Ok(None),
        }
    } else {
        let total_len = header_len + content_length;
        if buffer.len() < total_len {
            return Ok(None);
        }
        (buffer[header_len..total_len].to_vec(), total_len)
    };

    let remaining = buffer.split_off(consumed);
    buffer.clear();
    buffer.extend_from_slice(&remaining);

    request.body = body;

    Ok(Some(request))
}

// Intent: calculate connection persistence from HTTP headers and version.
// Result: the server retains keep-alive compatibility for HTTP/1.0 and 1.1.
pub(crate) fn should_keep_alive(version: u8, headers: &[(String, String)]) -> bool {
    let connection = header_value_impl(headers, "Connection")
        .unwrap_or_default()
        .to_ascii_lowercase();
    if version == 1 {
        !connection.contains("close")
    } else {
        connection.contains("keep-alive")
    }
}

// Intent: fetch a header value from parsed header pairs.
// Result: callers can inspect headers without duplicating matching logic.
pub(crate) fn header_value(headers: &[(String, String)], name: &str) -> Option<String> {
    header_value_impl(headers, name)
}
