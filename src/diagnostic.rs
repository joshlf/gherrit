//! Bounded, terminal-safe detail for human diagnostics.

pub(crate) fn detail(value: &str) -> String {
    const MAX_BYTES: usize = 160;

    let mut rendered = String::new();
    for character in value.chars() {
        let escaped = character.escape_default().to_string();
        if rendered.len() + escaped.len() > MAX_BYTES {
            rendered.push('…');
            break;
        }
        rendered.push_str(&escaped);
    }
    rendered
}
