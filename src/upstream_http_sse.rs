use std::mem;

pub(super) const FRAME_LIMIT: usize = 1024 * 1024;

#[derive(Debug)]
pub(super) struct Event {
    pub name: String,
    pub data: String,
}

#[derive(Default)]
pub(super) struct Decoder {
    line: Vec<u8>,
    data: Vec<String>,
    name: String,
    frame_bytes: usize,
    after_carriage_return: bool,
    first_line: bool,
}

impl Decoder {
    pub fn new() -> Self {
        Self {
            first_line: true,
            ..Self::default()
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<Event>, String> {
        let mut events = Vec::new();
        for &byte in bytes {
            if self.after_carriage_return {
                self.after_carriage_return = false;
                if byte == b'\n' {
                    continue;
                }
            }
            self.frame_bytes += 1;
            if self.frame_bytes > FRAME_LIMIT {
                return Err("HTTP SSE frame exceeds size limit".into());
            }
            match byte {
                b'\r' | b'\n' => {
                    self.finish_line(&mut events)?;
                    self.after_carriage_return = byte == b'\r';
                }
                _ => self.line.push(byte),
            }
        }
        Ok(events)
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
        assert!(decoder.feed(b"\xef\xbb\xbf: heartbeat\r")?.is_empty());
        assert!(
            decoder
                .feed(b"\nevent: message\r\ndata: {\"id\":\r\ndata: 7}\r")?
                .is_empty()
        );
        let events = decoder.feed(b"\n\r\n")?;
        assert_eq!(events.len(), 1);
        let event = events.first().ok_or("missing event")?;
        assert_eq!(event.name, "message");
        assert_eq!(event.data, "{\"id\":\n7}");
        Ok(())
    }

    #[test]
    fn bounds_unterminated_frames() {
        let mut decoder = Decoder::new();
        assert!(decoder.feed(&vec![b'x'; FRAME_LIMIT + 1]).is_err());
    }
}
