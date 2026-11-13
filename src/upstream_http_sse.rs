use std::mem;

use super::MAX_RESPONSE_BYTES;

const MAX_RETAINED_LINE_BYTES: usize = 64 * 1024;

#[derive(Debug)]
pub(super) struct Event {
    pub name: String,
    pub data: String,
}

/// Preserves completed events that precede an error in the same input chunk.
#[must_use]
pub(super) struct Feed {
    pub events: Vec<Event>,
    pub error: Option<String>,
}

#[derive(Default)]
pub(super) struct Decoder {
    line: Vec<u8>,
    data: String,
    name: String,
    frame_bytes: usize,
    after_carriage_return: bool,
    first_line: bool,
    terminated: bool,
}

impl Decoder {
    pub fn new() -> Self {
        Self {
            first_line: true,
            ..Self::default()
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Feed {
        let mut feed = Feed {
            events: Vec::new(),
            error: None,
        };
        if self.terminated {
            return feed;
        }
        for &byte in bytes {
            if self.after_carriage_return {
                self.after_carriage_return = false;
                if byte == b'\n' {
                    continue;
                }
            }
            self.frame_bytes += 1;
            if self.frame_bytes > MAX_RESPONSE_BYTES {
                feed.error = Some(format!(
                    "HTTP SSE frame exceeds size limit ({MAX_RESPONSE_BYTES} bytes)"
                ));
                break;
            }
            match byte {
                b'\r' | b'\n' => {
                    if let Err(error) = self.finish_line(&mut feed.events) {
                        feed.error = Some(error);
                        break;
                    }
                    self.after_carriage_return = byte == b'\r';
                }
                _ => self.line.push(byte),
            }
        }
        self.terminated = feed.error.is_some();
        feed
    }

    fn finish_line(&mut self, events: &mut Vec<Event>) -> Result<(), String> {
        let mut line = match std::str::from_utf8(&self.line) {
            Ok(line) => line,
            Err(_) => {
                self.clear_line();
                return Err("HTTP SSE contains invalid UTF-8".into());
            }
        };
        if self.first_line {
            line = line.strip_prefix('\u{feff}').unwrap_or(line);
            self.first_line = false;
        }
        if line.is_empty() {
            if !self.data.is_empty() {
                self.data.pop();
                events.push(Event {
                    name: mem::take(&mut self.name),
                    data: mem::take(&mut self.data),
                });
            }
            self.name.clear();
            self.frame_bytes = 0;
        } else if !line.starts_with(':') {
            let (field, value) = line.split_once(':').unwrap_or((line, ""));
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "data" => {
                    self.data.push_str(value);
                    self.data.push('\n');
                }
                "event" => self.name = value.to_string(),
                _ => {}
            }
        }
        self.clear_line();
        Ok(())
    }

    fn clear_line(&mut self) {
        if self.line.capacity() > MAX_RETAINED_LINE_BYTES {
            self.line = Vec::new();
        } else {
            self.line.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_crlf_multiline_and_comments() -> Result<(), String> {
        let mut decoder = Decoder::new();
        let feed = decoder.feed(b"\xef\xbb\xbf: heartbeat\r");
        assert!(feed.events.is_empty());
        assert!(feed.error.is_none());
        assert!(
            decoder
                .feed(b"\nevent: message\r\ndata: {\"id\":\r\ndata: 7}\r")
                .events
                .is_empty()
        );
        let feed = decoder.feed(b"\n\r\n");
        assert!(feed.error.is_none());
        let events = feed.events;
        assert_eq!(events.len(), 1);
        let event = events.first().ok_or("missing event")?;
        assert_eq!(event.name, "message");
        assert_eq!(event.data, "{\"id\":\n7}");
        Ok(())
    }

    #[test]
    fn bom_is_only_stripped_from_the_first_line() {
        let bytes = "\u{feff}: heartbeat\n\n\u{feff}data: ignored\ndata: kept\n\n".as_bytes();
        for boundary in 0..=bytes.len() {
            let (first, second) = bytes.split_at(boundary);
            let mut decoder = Decoder::new();
            let first = decoder.feed(first);
            let second = decoder.feed(second);
            assert!(first.error.is_none(), "boundary {boundary}");
            assert!(second.error.is_none(), "boundary {boundary}");
            let events: Vec<Event> = first.events.into_iter().chain(second.events).collect();
            assert_eq!(events.len(), 1, "boundary {boundary}");
            assert_eq!(
                events
                    .first()
                    .map(|event| (event.name.as_str(), event.data.as_str())),
                Some(("", "kept")),
                "boundary {boundary}"
            );
        }
    }

    #[test]
    fn large_valid_line_then_small_event_across_chunks() {
        let payload = "x".repeat(MAX_RETAINED_LINE_BYTES + 1);
        let frame = format!("data: {payload}\n\ndata: small\n\n");
        let mut decoder = Decoder::new();
        let mut events = Vec::new();
        for chunk in frame.as_bytes().chunks(1023) {
            let feed = decoder.feed(chunk);
            assert!(feed.error.is_none());
            events.extend(feed.events);
        }
        assert_eq!(events.len(), 2);
        assert_eq!(
            events
                .first()
                .map(|event| (event.name.as_str(), event.data.as_str())),
            Some(("", payload.as_str()))
        );
        assert_eq!(
            events
                .last()
                .map(|event| (event.name.as_str(), event.data.as_str())),
            Some(("", "small"))
        );
    }

    #[test]
    fn completed_lines_retain_only_bounded_scratch_capacity() {
        for invalid_utf8 in [false, true] {
            let mut decoder = Decoder::new();
            let feed = decoder.feed(b": warmup\n");
            assert!(feed.error.is_none());
            assert!(decoder.line.capacity() > 0);
            assert!(decoder.line.capacity() <= MAX_RETAINED_LINE_BYTES);
            let mut line = vec![b':'; MAX_RETAINED_LINE_BYTES + 1];
            if invalid_utf8 {
                line.push(0xff);
            }
            line.push(b'\n');
            let feed = decoder.feed(&line);
            assert_eq!(
                feed.error.as_deref(),
                invalid_utf8.then_some("HTTP SSE contains invalid UTF-8")
            );
            assert!(feed.events.is_empty());
            assert!(decoder.line.is_empty());
            assert!(decoder.line.capacity() <= MAX_RETAINED_LINE_BYTES);
        }
    }

    #[test]
    fn sixteen_mib_limit_is_per_frame_and_rejects_the_next_byte() {
        let limit = 16 * 1024 * 1024;
        let frame = format!("data: {}\n\n", "x".repeat(limit - 8));
        let mut decoder = Decoder::new();
        for _ in 0..2 {
            let feed = decoder.feed(frame.as_bytes());
            assert!(feed.error.is_none());
            assert_eq!(feed.events.len(), 1);
            assert_eq!(
                feed.events.first().map(|event| event.data.len()),
                Some(limit - 8)
            );
        }
        let feed = decoder.feed(&vec![b'x'; limit]);
        assert!(feed.error.is_none());
        assert!(feed.events.is_empty());
        let feed = decoder.feed(b"x");
        assert_eq!(
            feed.error.as_deref(),
            Some("HTTP SSE frame exceeds size limit (16777216 bytes)")
        );
        assert!(feed.events.is_empty());
    }

    #[test]
    fn empty_data_lines_and_event_names_survive_every_chunk_boundary() {
        let bytes = "event: ignored\n\nid: ignored\ndata:\n\nevent: message\ndata\n: comment\ndata: λ\ndata:  spaced\ndata:\n\nevent: reset\n\ndata: last\n\n".as_bytes();
        for boundary in 0..=bytes.len() {
            let (first, second) = bytes.split_at(boundary);
            let mut decoder = Decoder::new();
            let first = decoder.feed(first);
            let second = decoder.feed(second);
            assert!(first.error.is_none(), "boundary {boundary}");
            assert!(second.error.is_none(), "boundary {boundary}");
            let events: Vec<(String, String)> = first
                .events
                .into_iter()
                .chain(second.events)
                .map(|event| (event.name, event.data))
                .collect();
            assert_eq!(
                events,
                vec![
                    (String::new(), String::new()),
                    ("message".into(), "\nλ\n spaced\n".into()),
                    (String::new(), "last".into()),
                ],
                "boundary {boundary}"
            );
        }
    }

    #[test]
    fn completed_events_survive_invalid_suffix_at_every_chunk_boundary() {
        for (suffix, expected_error) in [
            (
                vec![0xff, b'\n'],
                "HTTP SSE contains invalid UTF-8".to_owned(),
            ),
            (
                vec![b'x'; MAX_RESPONSE_BYTES + 1],
                format!("HTTP SSE frame exceeds size limit ({MAX_RESPONSE_BYTES} bytes)"),
            ),
        ] {
            let prefix = b"event: message\r\ndata: {\"id\":7,\"result\":{}}\r\n\r\n";
            let bytes = [prefix.as_slice(), suffix.as_slice()].concat();
            let boundaries =
                (0..=prefix.len() + 2).chain([bytes.len() / 2, bytes.len() - 1, bytes.len()]);
            for boundary in boundaries {
                let (first, second) = bytes.split_at(boundary);
                let mut decoder = Decoder::new();
                let first = decoder.feed(first);
                let second = decoder.feed(second);
                let events: Vec<Event> = first.events.into_iter().chain(second.events).collect();
                assert_eq!(events.len(), 1, "boundary {boundary}");
                assert_eq!(
                    events
                        .first()
                        .map(|event| (event.name.as_str(), event.data.as_str())),
                    Some(("message", "{\"id\":7,\"result\":{}}"))
                );
                let errors: Vec<String> = first.error.into_iter().chain(second.error).collect();
                assert_eq!(errors, vec![expected_error.clone()], "boundary {boundary}");
                let repeated = decoder.feed(prefix);
                assert!(repeated.events.is_empty());
                assert!(repeated.error.is_none());
            }
        }
    }
}
