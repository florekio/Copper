use std::borrow::Cow;
use std::io;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt};

use crate::NetError;

#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub reason: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn body_str(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(&self.body)
    }

    pub async fn read_from<R: AsyncBufRead + Unpin>(reader: &mut R) -> Result<Self, NetError> {
        let status_line = read_line(reader).await?;
        let (status, reason) = parse_status_line(&status_line)?;

        let mut headers: Vec<(String, String)> = Vec::new();
        loop {
            let line = read_line(reader).await?;
            if line.is_empty() {
                break;
            }
            // RFC 7230 obs-fold: a header line starting with whitespace is a continuation.
            if line.starts_with(' ') || line.starts_with('\t') {
                if let Some(last) = headers.last_mut() {
                    last.1.push(' ');
                    last.1.push_str(line.trim());
                    continue;
                }
                return Err(NetError::MalformedHeader(line));
            }
            let (name, value) = line
                .split_once(':')
                .ok_or_else(|| NetError::MalformedHeader(line.clone()))?;
            headers.push((name.trim().to_string(), value.trim().to_string()));
        }

        let body = read_body(reader, &headers, status).await?;

        Ok(Response {
            status,
            reason,
            headers,
            body,
        })
    }
}

fn parse_status_line(line: &str) -> Result<(u16, String), NetError> {
    let mut parts = line.splitn(3, ' ');
    let version = parts
        .next()
        .ok_or_else(|| NetError::MalformedStatusLine(line.to_string()))?;
    if !version.starts_with("HTTP/1.") {
        return Err(NetError::MalformedStatusLine(line.to_string()));
    }
    let code = parts
        .next()
        .ok_or_else(|| NetError::MalformedStatusLine(line.to_string()))?;
    let status: u16 = code
        .parse()
        .map_err(|_| NetError::MalformedStatusLine(line.to_string()))?;
    let reason = parts.next().unwrap_or("").to_string();
    Ok((status, reason))
}

async fn read_body<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    headers: &[(String, String)],
    status: u16,
) -> Result<Vec<u8>, NetError> {
    // Some statuses must not include a body.
    if matches!(status, 204 | 304) || (100..200).contains(&status) {
        return Ok(Vec::new());
    }

    let transfer_encoding = header_lower(headers, "transfer-encoding");
    if transfer_encoding
        .as_deref()
        .map(|v| v.split(',').any(|t| t.trim().eq_ignore_ascii_case("chunked")))
        .unwrap_or(false)
    {
        return read_chunked(reader).await;
    }

    if let Some(len_str) = header_lower(headers, "content-length") {
        let len: usize = len_str
            .trim()
            .parse()
            .map_err(|_| NetError::MalformedHeader(format!("Content-Length: {len_str}")))?;
        let mut buf = vec![0u8; len];
        reader.read_exact(&mut buf).await?;
        return Ok(buf);
    }

    // No framing — read until EOF (Connection: close case).
    let mut buf = Vec::new();
    reader.read_to_end(&mut buf).await?;
    Ok(buf)
}

async fn read_chunked<R: AsyncBufRead + Unpin>(reader: &mut R) -> Result<Vec<u8>, NetError> {
    let mut body = Vec::new();
    loop {
        let line = read_line(reader).await?;
        // Chunk extensions after `;` are ignored.
        let size_token = line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_token, 16)
            .map_err(|_| NetError::MalformedChunk(line.clone()))?;
        if size == 0 {
            // Trailers (possibly empty), terminated by blank line.
            loop {
                let trailer = read_line(reader).await?;
                if trailer.is_empty() {
                    break;
                }
            }
            return Ok(body);
        }
        let start = body.len();
        body.resize(start + size, 0);
        reader.read_exact(&mut body[start..]).await?;
        let crlf = read_line(reader).await?;
        if !crlf.is_empty() {
            return Err(NetError::MalformedChunk(format!(
                "expected CRLF after chunk, got {crlf:?}"
            )));
        }
    }
}

async fn read_line<R: AsyncBufRead + Unpin>(reader: &mut R) -> io::Result<String> {
    let mut buf = Vec::new();
    let n = reader.read_until(b'\n', &mut buf).await?;
    if n == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "connection closed before line",
        ));
    }
    if buf.ends_with(b"\r\n") {
        buf.truncate(buf.len() - 2);
    } else if buf.ends_with(b"\n") {
        buf.truncate(buf.len() - 1);
    }
    String::from_utf8(buf).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidData, "non-UTF-8 byte in HTTP header")
    })
}

fn header_lower(headers: &[(String, String)], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use tokio::io::BufReader;

    fn run<F: std::future::Future>(f: F) -> F::Output {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(f)
    }

    #[test]
    fn parses_content_length() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nContent-Type: text/plain\r\n\r\nhello";
        let mut reader = BufReader::new(Cursor::new(raw.to_vec()));
        let resp = run(Response::read_from(&mut reader)).unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.reason, "OK");
        assert_eq!(resp.body, b"hello");
        assert_eq!(resp.header("content-type"), Some("text/plain"));
    }

    #[test]
    fn parses_chunked() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        let mut reader = BufReader::new(Cursor::new(raw.to_vec()));
        let resp = run(Response::read_from(&mut reader)).unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, b"hello world");
    }

    #[test]
    fn parses_chunked_with_extension() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5;ext=foo\r\nhello\r\n0\r\n\r\n";
        let mut reader = BufReader::new(Cursor::new(raw.to_vec()));
        let resp = run(Response::read_from(&mut reader)).unwrap();
        assert_eq!(resp.body, b"hello");
    }

    #[test]
    fn no_body_on_204() {
        let raw = b"HTTP/1.1 204 No Content\r\n\r\n";
        let mut reader = BufReader::new(Cursor::new(raw.to_vec()));
        let resp = run(Response::read_from(&mut reader)).unwrap();
        assert_eq!(resp.status, 204);
        assert!(resp.body.is_empty());
    }

    #[test]
    fn read_to_eof_when_no_framing() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\r\nhello, world";
        let mut reader = BufReader::new(Cursor::new(raw.to_vec()));
        let resp = run(Response::read_from(&mut reader)).unwrap();
        assert_eq!(resp.body, b"hello, world");
    }

    #[test]
    fn folded_header() {
        let raw =
            b"HTTP/1.1 200 OK\r\nX-Long: foo\r\n bar\r\nContent-Length: 0\r\n\r\n";
        let mut reader = BufReader::new(Cursor::new(raw.to_vec()));
        let resp = run(Response::read_from(&mut reader)).unwrap();
        assert_eq!(resp.header("X-Long"), Some("foo bar"));
    }
}
