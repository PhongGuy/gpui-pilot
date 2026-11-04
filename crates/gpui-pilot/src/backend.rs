//! The request-reply exchange with a launched host over its Unix socket.

use std::io::{BufRead, BufReader, ErrorKind};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use gpui_pilot_protocol::{BackendFrame, ClientFrame, Outcome, Request};

#[derive(Debug, PartialEq)]
pub(crate) enum BackendError {
    /// The host closed the socket, or a write failed: it has exited or is exiting.
    Closed,
    /// No reply within the limit; the host is still connected.
    Timeout,
    /// A line that is not a `BackendFrame`.
    Malformed(String),
}

pub(crate) struct Backend {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
    /// Bytes of a reply line not yet terminated; kept across a timeout.
    pending: Vec<u8>,
    next_id: u64,
}

impl Backend {
    pub fn new(stream: UnixStream) -> std::io::Result<Backend> {
        let reader = BufReader::new(stream.try_clone()?);
        Ok(Backend {
            writer: stream,
            reader,
            pending: Vec::new(),
            next_id: 1,
        })
    }

    /// Sends `request` and waits up to `limit` for the reply carrying its id. Replies to earlier
    /// requests that timed out arrive first and are discarded.
    pub fn call(&mut self, request: Request, limit: Duration) -> Result<Outcome, BackendError> {
        let id = self.next_id;
        self.next_id += 1;
        gpui_pilot_protocol::write_line(&mut self.writer, &ClientFrame::new(id, request))
            .map_err(|_| BackendError::Closed)?;
        let deadline = Instant::now() + limit;
        // The protocol's read_line would drop a half-read line on timeout, so the partial
        // bytes stay in `pending` and the next call resumes mid-frame.
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(BackendError::Timeout);
            }
            self.reader
                .get_ref()
                .set_read_timeout(Some(left))
                .map_err(|_| BackendError::Closed)?;
            match self.reader.read_until(b'\n', &mut self.pending) {
                Ok(0) => return Err(BackendError::Closed),
                Ok(_) if self.pending.last() != Some(&b'\n') => return Err(BackendError::Closed),
                Ok(_) => {
                    let line = std::mem::take(&mut self.pending);
                    let frame: BackendFrame = serde_json::from_slice(&line)
                        .map_err(|e| BackendError::Malformed(e.to_string()))?;
                    if frame.id == id {
                        return Ok(frame.outcome);
                    }
                }
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    return Err(BackendError::Timeout);
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(_) => return Err(BackendError::Closed),
            }
        }
    }

    /// Shuts both directions down, so the host sees end of stream.
    pub fn shutdown(&self) {
        let _ = self.writer.shutdown(Shutdown::Both);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufReader, Write};
    use std::thread;

    use gpui_pilot_protocol::{ClientFrame, Reply, read_line, write_line};

    /// A fake host on the other end of a socket pair, running `script` on its stream.
    fn fake_host(
        script: impl FnOnce(BufReader<UnixStream>, UnixStream) + Send + 'static,
    ) -> Backend {
        let (ours, theirs) = UnixStream::pair().unwrap();
        thread::spawn(move || {
            let reader = BufReader::new(theirs.try_clone().unwrap());
            script(reader, theirs);
        });
        Backend::new(ours).unwrap()
    }

    fn key() -> Request {
        Request::Key {
            keys: "escape".into(),
            settle_ms: 0,
        }
    }

    #[test]
    fn a_reply_is_matched_to_its_request() {
        let mut backend = fake_host(|mut reader, mut writer| {
            while let Some(frame) = read_line::<ClientFrame>(&mut reader).unwrap() {
                let outcome = Outcome::Ok(Reply::Key {
                    settled: frame.id == 2,
                });
                write_line(&mut writer, &BackendFrame::new(frame.id, outcome)).unwrap();
            }
        });
        let limit = Duration::from_secs(5);
        assert_eq!(
            backend.call(key(), limit),
            Ok(Outcome::Ok(Reply::Key { settled: false }))
        );
        assert_eq!(
            backend.call(key(), limit),
            Ok(Outcome::Ok(Reply::Key { settled: true }))
        );
    }

    #[test]
    fn a_late_reply_is_discarded_after_a_timeout() {
        let mut backend = fake_host(|mut reader, mut writer| {
            let first: ClientFrame = read_line(&mut reader).unwrap().unwrap();
            let second: ClientFrame = read_line(&mut reader).unwrap().unwrap();
            for (id, settled) in [(first.id, false), (second.id, true)] {
                let outcome = Outcome::Ok(Reply::Key { settled });
                write_line(&mut writer, &BackendFrame::new(id, outcome)).unwrap();
            }
            thread::sleep(Duration::from_millis(200));
        });
        assert_eq!(
            backend.call(key(), Duration::from_millis(50)),
            Err(BackendError::Timeout)
        );
        let outcome = backend.call(key(), Duration::from_secs(5));
        assert_eq!(outcome, Ok(Outcome::Ok(Reply::Key { settled: true })));
    }

    #[test]
    fn a_partial_line_survives_a_timeout() {
        let mut backend = fake_host(|mut reader, mut writer| {
            let first: ClientFrame = read_line(&mut reader).unwrap().unwrap();
            let line = format!(
                "{{\"id\":{},\"outcome\":{{\"ok\":{{\"op\":\"key\",\"settled\":false}}}}}}\n",
                first.id
            );
            let (head, tail) = line.split_at(10);
            writer.write_all(head.as_bytes()).unwrap();
            thread::sleep(Duration::from_millis(150));
            writer.write_all(tail.as_bytes()).unwrap();
            let second: ClientFrame = read_line(&mut reader).unwrap().unwrap();
            let outcome = Outcome::Ok(Reply::Key { settled: true });
            write_line(&mut writer, &BackendFrame::new(second.id, outcome)).unwrap();
            thread::sleep(Duration::from_millis(200));
        });
        assert_eq!(
            backend.call(key(), Duration::from_millis(50)),
            Err(BackendError::Timeout)
        );
        let outcome = backend.call(key(), Duration::from_secs(5));
        assert_eq!(outcome, Ok(Outcome::Ok(Reply::Key { settled: true })));
    }

    #[test]
    fn a_closed_socket_and_a_bad_frame_are_reported() {
        let mut closed = fake_host(|_, _| {});
        assert_eq!(
            closed.call(key(), Duration::from_secs(5)),
            Err(BackendError::Closed)
        );
        let mut junk = fake_host(|mut reader, mut writer| {
            let _: Option<ClientFrame> = read_line(&mut reader).unwrap();
            writer.write_all(b"{not json\n").unwrap();
            thread::sleep(Duration::from_millis(200));
        });
        assert!(matches!(
            junk.call(key(), Duration::from_secs(5)),
            Err(BackendError::Malformed(_))
        ));
    }
}
