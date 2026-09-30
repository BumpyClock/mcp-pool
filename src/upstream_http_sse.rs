use std::mem;

pub(super) const FRAME_LIMIT: usize = 1024 * 1024;

#[derive(Debug)]
pub(super) struct Event {
    pub name: String,
    pub data: String,
}

// Completed events precede the terminal error, including within one transport chunk.
#[must_use]
pub(super) struct Feed {
    pub events: Vec<Event>,
    pub error: Option<String>,
}

#[derive(Default)]
pub(super) struct Decoder {
    line: Vec<u8>,
    data: Vec<String>,
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
            if self.frame_bytes > FRAME_LIMIT {
                feed.error = Some("HTTP SSE frame exceeds size limit".into());
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
        let bytes = mem::take(&mut self.line);
        let mut line =
            std::str::from_utf8(&bytes).map_err(|_| "HTTP SSE contains invalid UTF-8")?;
        if self.first_line {
            line = line.strip_prefix('\u{feff}').unwrap_or(line);
            self.first_line = false;
        }
        if line.is_empty() {
            if !self.data.is_empty() {
                events.push(Event {
                    name: mem::take(&mut self.name),
                    data: mem::take(&mut self.data).join("\n"),
                });
            }
            self.name.clear();
            self.frame_bytes = 0;
            return Ok(());
        }
        if line.starts_with(':') {
            return Ok(());
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "data" => self.data.push(value.to_string()),
            "event" => self.name = value.to_string(),
            _ => {}
        }
        Ok(())
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
    fn bounds_unterminated_frames() {
        let mut decoder = Decoder::new();
        assert!(decoder.feed(&vec![b'x'; FRAME_LIMIT + 1]).error.is_some());
    }

    #[test]
    fn completed_events_survive_invalid_suffix_at_every_chunk_boundary() {
        for (suffix, expected_error) in [
            (vec![0xff, b'\n'], "HTTP SSE contains invalid UTF-8"),
            (
                vec![b'x'; FRAME_LIMIT + 1],
                "HTTP SSE frame exceeds size limit",
            ),
        ] {
            let prefix = b"event: message\r\ndata: {\"id\":7,\"result\":{}}\r\n\r\n";
            let bytes = [prefix.as_slice(), suffix.as_slice()].concat();
            // Exercise every boundary around dispatch and UTF-8; sample the large frame's interior.
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
                assert_eq!(errors, vec![expected_error], "boundary {boundary}");
                let repeated = decoder.feed(prefix);
                assert!(repeated.events.is_empty());
                assert!(repeated.error.is_none());
            }
        }
    }
}
