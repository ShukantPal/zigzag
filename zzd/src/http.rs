use crate::logging;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use zz::{Json, ReadResult};

pub(crate) const MAX_BODY: usize = 64 * 1024;
/// Upper bound on the request line plus all header lines, in bytes.
pub(crate) const MAX_HEADER_BLOCK_BYTES: usize = 8 * 1024;
/// Upper bound on the number of header lines in one request.
pub(crate) const MAX_HEADER_COUNT: usize = 100;
pub(crate) struct Request {
    pub(crate) method: String,
    pub(crate) target: String,
    pub(crate) headers: HashMap<String, String>,
    pub(crate) body: Vec<u8>,
}
#[derive(Debug)]
pub(crate) enum ReadRequestError {
    Message(String),
    ExecutionDenied,
    HeadersTooLarge,
}
pub(crate) fn denied(stream: &mut TcpStream, id: &str) -> Result<(), String> {
    let (status, body) = denial_response(id);
    reply(stream, status, body)
}
pub(crate) fn denial_response(id: &str) -> (u16, Json) {
    (200, denial_json(id))
}
pub(crate) fn denial_json(id: &str) -> Json {
    Json::Object(vec![
        ("id".to_owned(), Json::String(id.to_owned())),
        ("error".to_owned(), Json::String("denied".to_owned())),
    ])
}
pub(crate) fn get_query(target: &str) -> Result<(u64, u64, String), String> {
    let query = query(target)?;
    let after = query.get("after").map_or(Ok(0), |value| {
        value
            .parse::<u64>()
            .map_err(|_| "after must be a non-negative integer".to_owned())
    })?;
    let timeout = query.get("timeout").map_or(Ok(50), |value| {
        value
            .parse::<u64>()
            .map_err(|_| "timeout must be an integer".to_owned())
    })?;
    if timeout > 55 {
        return Err("timeout must be between 0 and 55".to_owned());
    }
    Ok((
        after,
        timeout,
        query.get("epoch").cloned().unwrap_or_default(),
    ))
}
pub(crate) fn read_json(result: ReadResult) -> Json {
    Json::Object(vec![
        ("epoch".to_owned(), Json::String(result.epoch)),
        ("reset".to_owned(), Json::Bool(result.reset)),
        ("lost".to_owned(), Json::Bool(result.lost)),
        (
            "events".to_owned(),
            Json::Array(
                result
                    .events
                    .iter()
                    .map(|event| event.response_json())
                    .collect(),
            ),
        ),
        ("next".to_owned(), Json::number(result.next)),
    ])
}
pub(crate) fn read_request(stream: &mut TcpStream) -> Result<Request, ReadRequestError> {
    let mut reader = BufReader::new(stream);
    // The budget covers the request line too: an unbounded request line is
    // the same allocation attack as unbounded headers.
    let mut budget = MAX_HEADER_BLOCK_BYTES;
    let first = read_header_line(&mut reader, &mut budget)?
        .ok_or_else(|| ReadRequestError::Message("malformed request line".to_owned()))?;
    let mut parts = first.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| ReadRequestError::Message("malformed request line".to_owned()))?
        .to_owned();
    let target = parts
        .next()
        .ok_or_else(|| ReadRequestError::Message("malformed request line".to_owned()))?
        .to_owned();
    if parts.next().is_none() {
        return Err(ReadRequestError::Message(
            "malformed request line".to_owned(),
        ));
    }
    let mut headers = HashMap::new();
    let mut header_count = 0;
    loop {
        let line = read_header_line(&mut reader, &mut budget)?
            .ok_or_else(|| ReadRequestError::Message("could not read headers".to_owned()))?;
        if line == "\r\n" || line == "\n" {
            break;
        }
        // Count lines, not map entries: duplicate header names collapse in
        // the map, but each line still costs the peer nothing to send.
        header_count += 1;
        if header_count > MAX_HEADER_COUNT {
            return Err(ReadRequestError::HeadersTooLarge);
        }
        let (name, value) = line
            .trim_end()
            .split_once(':')
            .ok_or_else(|| ReadRequestError::Message("malformed header".to_owned()))?;
        headers.insert(name.to_ascii_lowercase(), value.trim().to_owned());
    }
    let length = headers.get("content-length").map_or(Ok(0), |value| {
        value
            .parse::<usize>()
            .map_err(|_| ReadRequestError::Message("invalid content length".to_owned()))
    })?;
    if length > MAX_BODY {
        if method == "POST"
            && matches!(
                target.split('?').next(),
                Some("/v1/exec") | Some("/v1/spawn")
            )
        {
            return Err(ReadRequestError::ExecutionDenied);
        }
        return Err(ReadRequestError::Message(
            "request body too large".to_owned(),
        ));
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).map_err(|_| {
        if method == "POST"
            && matches!(
                target.split('?').next(),
                Some("/v1/exec") | Some("/v1/spawn")
            )
        {
            ReadRequestError::ExecutionDenied
        } else {
            ReadRequestError::Message("short request body".to_owned())
        }
    })?;
    Ok(Request {
        method,
        target,
        headers,
        body,
    })
}
/// Read one header line, deducting its bytes from `budget`.
///
/// The budget caps the whole header block (request line included). Reads go
/// through `take(budget)`, so a peer can never make the block larger than the
/// cap no matter how long a single line is: `read_until` returns as soon as
/// the budget is consumed, even without a line terminator. `None` is a clean
/// EOF before any byte of the line.
pub(crate) fn read_header_line(
    reader: &mut BufReader<&mut TcpStream>,
    budget: &mut usize,
) -> Result<Option<String>, ReadRequestError> {
    if *budget == 0 {
        return Err(ReadRequestError::HeadersTooLarge);
    }
    let mut line = Vec::new();
    let consumed = reader
        .by_ref()
        .take(*budget as u64)
        .read_until(b'\n', &mut line)
        .map_err(|_| ReadRequestError::Message("could not read headers".to_owned()))?;
    *budget -= consumed;
    if consumed == 0 {
        return Ok(None);
    }
    if !line.ends_with(b"\n") {
        // Budget exhausted before the terminator: the header block is over
        // the cap. (A peer that disconnects mid-header lands here too; it is
        // still a client error, as before.)
        return Err(ReadRequestError::HeadersTooLarge);
    }
    String::from_utf8(line)
        .map(Some)
        .map_err(|_| ReadRequestError::Message("malformed header".to_owned()))
}
pub(crate) fn query(target: &str) -> Result<HashMap<String, String>, String> {
    let Some((_, raw)) = target.split_once('?') else {
        return Ok(HashMap::new());
    };
    let pairs: Vec<_> = raw
        .split('&')
        .filter(|value| !value.is_empty())
        .map(|item| {
            let (key, value) = item.split_once('=').unwrap_or((item, ""));
            Ok::<_, String>((percent_decode(key)?, percent_decode(value)?))
        })
        .collect::<Result<_, _>>()?;
    let mut values = HashMap::new();
    for (key, value) in pairs {
        if values.insert(key, value).is_some() {
            return Err("duplicate query parameter".to_owned());
        }
    }
    Ok(values)
}
pub(crate) fn percent_decode(input: &str) -> Result<String, String> {
    // Query strings use form-encoding, where a literal '+' means space.
    // Translate '+' first so an encoded "%2B" still decodes to '+'.
    let translated = input.replace('+', " ");
    // The percent-encoding crate passes malformed '%' sequences through
    // untouched instead of failing, so validate escapes up front. This keeps
    // the old strict contract: query values feed agent/log lookups and event
    // reads on the auth boundary, and bad input must be a 400, never silently
    // accepted.
    let mut bytes = translated.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let valid = bytes.next().is_some_and(|byte| byte.is_ascii_hexdigit())
                && bytes.next().is_some_and(|byte| byte.is_ascii_hexdigit());
            if !valid {
                return Err("invalid URL encoding".to_owned());
            }
        }
    }
    percent_encoding::percent_decode_str(&translated)
        .decode_utf8()
        .map(|decoded| decoded.into_owned())
        .map_err(|_| "invalid URL encoding".to_owned())
}
pub(crate) fn error(message: &str) -> Json {
    Json::Object(vec![("error".to_owned(), Json::String(message.to_owned()))])
}
pub(crate) fn reply(stream: &mut TcpStream, code: u16, value: Json) -> Result<(), String> {
    // Log response completion with status code and duration.
    let log_source = stream
        .peer_addr()
        .map(|addr| addr.ip().to_string())
        .unwrap_or_else(|_| "unknown".to_owned());
    logging::log_response(code, &log_source);
    let body = value.to_json();
    let reason = match code {
        200 => "OK",
        201 => "Created",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        409 => "Conflict",
        431 => "Request Header Fields Too Large",
        503 => "Service Unavailable",
        _ => "Internal Server Error",
    };
    stream.write_all(format!("HTTP/1.1 {code} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).map_err(|error| error.to_string())
}
