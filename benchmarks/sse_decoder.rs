use std::hint::black_box;
use std::time::Instant;

const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
#[path = "../src/upstream_http_sse.rs"]
mod decoder;

fn main() -> Result<(), String> {
    let frame = b": heartbeat\nevent: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"text\":\"fixture\"}}\n\n";
    let expected = "{\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"text\":\"fixture\"}}";
    let frames = 100_000;
    let input = frame.repeat(frames);
    for chunk_bytes in [64, 1024, 16384] {
        for sample in 0..8 {
            let mut parser = decoder::Decoder::new();
            let start = Instant::now();
            let mut events = 0;
            let mut payload_bytes = 0;
            for chunk in input.chunks(chunk_bytes) {
                let feed = parser.feed(black_box(chunk));
                if let Some(error) = feed.error {
                    return Err(error);
                }
                events += feed.events.len();
                for event in feed.events {
                    if event.name != "message" || event.data != expected {
                        return Err("Unexpected event name or payload".into());
                    }
                    payload_bytes += black_box(event.data).len();
                }
            }
            let elapsed = start.elapsed().as_nanos();
            if events != frames || payload_bytes != frames * expected.len() {
                return Err(format!(
                    "Incorrect output: {events} events, {payload_bytes} bytes"
                ));
            }
            if sample > 0 {
                println!(
                    "{{\"chunk_bytes\":{chunk_bytes},\"sample\":{sample},\"frames\":{frames},\"elapsed_ns\":{elapsed}}}"
                );
            }
        }
    }
    Ok(())
}
