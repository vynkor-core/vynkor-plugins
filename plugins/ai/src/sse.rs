//! Server-sent-events framing for streamed provider responses (CD-03).
//!
//! `network` hands the body over in arbitrary byte chunks — an event, a
//! line, even a UTF-8 sequence can be split across two of them — so this
//! buffers bytes and only yields complete events (terminated by a blank
//! line). Only `data:` payloads are surfaced: both anthropic and
//! openai-compatible streams repeat the event kind inside the JSON, and
//! comments/`event:`/`id:`/`retry:` lines carry nothing `ai` needs.

/// Cap on one buffered, not-yet-terminated event. A provider that never
/// sends a blank line would otherwise grow this without bound.
const MAX_PENDING_BYTES: usize = 1024 * 1024;

#[derive(Default)]
pub struct SseDecoder {
    buf: Vec<u8>,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one body chunk; returns the `data` of every event it completed,
    /// multi-line data joined with `\n` as the SSE spec says.
    pub fn feed(&mut self, chunk: &[u8]) -> Result<Vec<String>, String> {
        self.buf.extend_from_slice(chunk);
        let mut events = Vec::new();
        while let Some((end, sep_len)) = find_event_end(&self.buf) {
            let raw: Vec<u8> = self.buf.drain(..end + sep_len).take(end).collect();
            let text = String::from_utf8_lossy(&raw);
            let mut data: Vec<&str> = Vec::new();
            for line in text.lines() {
                if let Some(rest) = line.strip_prefix("data:") {
                    data.push(rest.strip_prefix(' ').unwrap_or(rest));
                }
            }
            if !data.is_empty() {
                events.push(data.join("\n"));
            }
        }
        if self.buf.len() > MAX_PENDING_BYTES {
            return Err("stream event exceeds 1 MiB without a terminator".into());
        }
        Ok(events)
    }
}

/// Index of the first blank-line terminator and its length; accepts
/// `\n\n`, `\r\n\r\n` and `\r\r` (all legal SSE line endings).
fn find_event_end(buf: &[u8]) -> Option<(usize, usize)> {
    for i in 0..buf.len() {
        for sep in [&b"\r\n\r\n"[..], b"\n\n", b"\r\r"] {
            if buf[i..].starts_with(sep) {
                return Some((i, sep.len()));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_split_across_chunks_are_reassembled() {
        let mut d = SseDecoder::new();
        assert!(d.feed(b"data: {\"a\"").unwrap().is_empty());
        assert!(d.feed(b":1}\n").unwrap().is_empty());
        assert_eq!(d.feed(b"\ndata: x\n\n").unwrap(), vec!["{\"a\":1}", "x"]);
    }

    #[test]
    fn crlf_event_names_comments_and_multiline_data() {
        let mut d = SseDecoder::new();
        let got = d
            .feed(b": ping\r\n\r\nevent: message_start\r\ndata: a\r\ndata: b\r\n\r\n")
            .unwrap();
        assert_eq!(got, vec!["a\nb"]);
    }

    #[test]
    fn utf8_split_mid_codepoint_survives() {
        let mut d = SseDecoder::new();
        let text = "data: привет\n\n".as_bytes();
        let (a, b) = text.split_at(8); // inside the first cyrillic letter
        assert!(d.feed(a).unwrap().is_empty());
        assert_eq!(d.feed(b).unwrap(), vec!["привет"]);
    }

    #[test]
    fn unterminated_event_is_capped() {
        let mut d = SseDecoder::new();
        let big = vec![b'x'; MAX_PENDING_BYTES + 1];
        assert!(d.feed(&big).is_err());
    }
}
