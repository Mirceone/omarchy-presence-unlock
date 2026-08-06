//! Control-socket client. The CLI never reads presence state itself.

use omarchy_presence_unlock_protocol::{paths::current_socket_path, wire};
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    time::Duration,
};

fn connect(timeout: Duration) -> Result<UnixStream, String> {
    let stream = UnixStream::connect(current_socket_path()).map_err(|error| {
        format!("cannot reach the daemon socket ({error}); is omarchy-presence-unlockd running?")
    })?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|e| e.to_string())?;
    Ok(stream)
}

/// Sends a request and reads exactly one response line.
///
/// # Errors
///
/// Returns a rendered error when the socket is unreachable or times out.
pub fn request(payload: &str, timeout: Duration) -> Result<String, String> {
    let mut stream = connect(timeout)?;
    stream
        .write_all(payload.as_bytes())
        .map_err(|e| e.to_string())?;
    let mut response = String::new();
    BufReader::new(stream)
        .read_line(&mut response)
        .map_err(|e| e.to_string())?;
    Ok(response)
}

/// Sends a request and reads lines until the terminator.
///
/// # Errors
///
/// Returns a rendered error when the socket is unreachable, times out, or the
/// daemon closes the connection before sending the terminator.
pub fn request_lines(payload: &str, timeout: Duration) -> Result<Vec<String>, String> {
    let mut stream = connect(timeout)?;
    stream
        .write_all(payload.as_bytes())
        .map_err(|e| e.to_string())?;
    let mut lines = Vec::new();
    for line in BufReader::new(stream).lines() {
        let line = line.map_err(|e| e.to_string())?;
        if line == wire::RESP_END.trim_end() {
            return Ok(lines);
        }
        lines.push(line);
    }
    Err("daemon closed the connection before the response ended".into())
}
