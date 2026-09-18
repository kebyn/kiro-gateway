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

#[derive(Clone, Debug, Default)]
pub struct XmlLeakFilter {
    in_tag: bool,
}

impl XmlLeakFilter {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn push(&mut self, input: &str) -> String {
        let mut output = String::with_capacity(input.len());
        for character in input.chars() {
            if self.in_tag {
                if character == '>' {
                    self.in_tag = false;
                }
            } else if character == '<' {
                self.in_tag = true;
            } else {
                output.push(character);
            }
        }
        output
    }
    pub fn finish(&mut self) -> String {
        self.in_tag = false;
        String::new()
    }
}
pub fn detect_incomplete_json(input: &str) -> bool {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return false;
    }
    serde_json::from_str::<serde_json::Value>(trimmed).is_err()
        && (trimmed.starts_with('{') || trimmed.starts_with('['))
}

#[cfg(test)]
mod tests {
    use super::XmlLeakFilter;
    #[test]
    fn filters_xml_tags_across_chunks() {
        let mut filter = XmlLeakFilter::new();
        assert_eq!(filter.push("before <tool"), "before ");
        assert_eq!(filter.push("_use>{\"x\":1}</tool_use> after"), "{\"x\":1} after");
    }
}
