use serde_json::Value;

/// Repair raw control characters in strings and trailing commas only, without evaluating JS.
pub fn lenient_json(text: &str) -> Option<Value> {
    if let Ok(v) = serde_json::from_str(text) {
        return Some(v);
    }
    let mut out = String::with_capacity(text.len());
    let mut quoted = false;
    let mut escaped = false;
    let chars: Vec<char> = text.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if quoted {
            if escaped {
                escaped = false;
                out.push(c);
                continue;
            }
            match c {
                '\\' => escaped = true,
                '"' => quoted = false,
                c if c < ' ' => {
                    out.push_str(&format!("\\u{:04x}", c as u32));
                    continue;
                }
                _ => {}
            }
        } else if c == '"' {
            quoted = true;
        } else if c == ','
            && chars[i + 1..]
                .iter()
                .find(|c| !c.is_whitespace())
                .is_some_and(|c| matches!(c, '}' | ']'))
        {
            continue;
        }
        out.push(c);
    }
    serde_json::from_str(&out).ok()
}
