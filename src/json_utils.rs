use serde_json::Value;

pub(crate) fn extract_json_payload(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    if serde_json::from_str::<Value>(trimmed).is_ok() {
        return Some(trimmed.to_string());
    }
    extract_first_json_value_segment(trimmed)
}

fn extract_first_json_value_segment(text: &str) -> Option<String> {
    for (start, ch) in text.char_indices() {
        if !matches!(ch, '{' | '[') {
            continue;
        }
        let suffix = &text[start..];
        let Some(end_offset) = find_json_segment_end(suffix) else {
            continue;
        };
        let candidate = suffix[..end_offset].trim();
        if serde_json::from_str::<Value>(candidate).is_ok() {
            return Some(candidate.to_string());
        }
    }
    None
}

fn find_json_segment_end(input: &str) -> Option<usize> {
    let mut stack = Vec::new();
    let mut in_string = false;
    let mut escaped = false;

    for (offset, ch) in input.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
                continue;
            }
            match ch {
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }

        match ch {
            '"' => in_string = true,
            '{' | '[' => stack.push(ch),
            '}' => {
                if stack.pop() != Some('{') {
                    return None;
                }
                if stack.is_empty() {
                    return Some(offset + ch.len_utf8());
                }
            }
            ']' => {
                if stack.pop() != Some('[') {
                    return None;
                }
                if stack.is_empty() {
                    return Some(offset + ch.len_utf8());
                }
            }
            _ => {}
        }
    }

    None
}
