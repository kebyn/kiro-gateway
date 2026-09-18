pub fn strip_tool_xml(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut in_tag = false;
    for character in input.chars() {
        if character == '<' {
            in_tag = true;
            continue;
        }
        if in_tag {
            if character == '>' {
                in_tag = false;
            }
            continue;
        }
        output.push(character);
    }
    output
}
pub fn detect_incomplete_json(input: &str) -> bool {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return false;
    }
    serde_json::from_str::<serde_json::Value>(trimmed).is_err()
        && (trimmed.starts_with('{') || trimmed.starts_with('['))
}
