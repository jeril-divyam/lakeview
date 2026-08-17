//! Deciding what fetched bytes are: text, a JSON document, JSONL records,
//! or something binary worth no more than a hex dump.

use super::PreviewBody;

/// Whether `path` names newline-delimited JSON. Decided by extension rather
/// than by sniffing: a `.jsonl` full of broken records is still JSONL, and
/// should say which records are broken instead of quietly rendering as text.
fn is_jsonl(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".jsonl") || lower.ends_with(".ndjson")
}

/// Decide whether the fetched bytes are text; fall back to a hex dump.
pub(super) fn render_body(path: &str, bytes: &[u8], truncated: bool) -> PreviewBody {
    let sample = &bytes[..bytes.len().min(8192)];
    let binary = sample.contains(&0)
        || String::from_utf8_lossy(sample)
            .chars()
            .filter(|c| *c == '\u{fffd}')
            .count()
            > 4;

    if binary {
        return PreviewBody::Binary(hex_dump(&bytes[..bytes.len().min(4096)]));
    }
    let text = String::from_utf8_lossy(bytes);

    // One record per line, each folded up. Unlike whole-file JSON this survives
    // truncation — every record but the last still parses on its own.
    if is_jsonl(path) {
        return PreviewBody::Jsonl(crate::jsonl::parse(&text, truncated));
    }

    // Pretty-print JSON. A body truncated by `preview_bytes` won't parse, so
    // this quietly falls through to the plain-text path.
    let head = text.trim_start();
    if (head.starts_with('{') || head.starts_with('['))
        && let Ok(value) = serde_json::from_str::<serde_json::Value>(&text)
    {
        return PreviewBody::Json(crate::jsonl::JsonDoc::new(value));
    }

    PreviewBody::Text(text.lines().map(|l| l.replace('\t', "    ")).collect())
}

fn hex_dump(bytes: &[u8]) -> Vec<String> {
    bytes
        .chunks(16)
        .enumerate()
        .map(|(i, chunk)| {
            let hex: Vec<String> = chunk.iter().map(|b| format!("{b:02x}")).collect();
            let ascii: String = chunk
                .iter()
                .map(|b| {
                    if b.is_ascii_graphic() || *b == b' ' {
                        *b as char
                    } else {
                        '.'
                    }
                })
                .collect();
            format!("{:08x}  {:<47}  {}", i * 16, hex.join(" "), ascii)
        })
        .collect()
}
