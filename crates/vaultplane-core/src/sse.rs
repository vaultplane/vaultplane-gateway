//! Minimal Server-Sent Events parser, shared by streaming provider connectors.
//!
//! Bytes are fed in as they arrive from the upstream; whole events are pulled out
//! when complete (a blank line, `\n\n`, terminates an event). Each event exposes
//! its optional `event:` name and the concatenated `data:` lines.

#[derive(Debug, Clone)]
pub(crate) struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

#[derive(Default)]
pub(crate) struct SseParser {
    buffer: Vec<u8>,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append more bytes from the upstream stream.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    /// Pull the next complete event from the buffer, if one is fully available.
    pub fn next_event(&mut self) -> Option<SseEvent> {
        let pos = self.buffer.windows(2).position(|w| w == b"\n\n")?;
        let text = std::str::from_utf8(&self.buffer[..pos]).ok()?.to_owned();
        self.buffer.drain(..pos + 2);
        parse(&text)
    }
}

fn parse(text: &str) -> Option<SseEvent> {
    let mut event = None;
    let mut data = String::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("event:") {
            event = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
        }
        // Comments (lines starting with `:`) and unknown fields are ignored.
    }
    Some(SseEvent { event, data })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_complete_events_from_one_chunk() {
        let mut p = SseParser::new();
        p.feed(b"event: ping\ndata: hi\n\nevent: done\ndata: bye\n\n");
        let e1 = p.next_event().unwrap();
        assert_eq!(e1.event.as_deref(), Some("ping"));
        assert_eq!(e1.data, "hi");
        let e2 = p.next_event().unwrap();
        assert_eq!(e2.event.as_deref(), Some("done"));
        assert_eq!(e2.data, "bye");
        assert!(p.next_event().is_none());
    }

    #[test]
    fn handles_data_only_events() {
        let mut p = SseParser::new();
        p.feed(b"data: {\"foo\":1}\n\n");
        let e = p.next_event().unwrap();
        assert!(e.event.is_none());
        assert_eq!(e.data, "{\"foo\":1}");
    }

    #[test]
    fn buffers_partial_chunks_across_feeds() {
        let mut p = SseParser::new();
        p.feed(b"data: par");
        assert!(p.next_event().is_none());
        p.feed(b"tial\n\n");
        let e = p.next_event().unwrap();
        assert_eq!(e.data, "partial");
    }
}
