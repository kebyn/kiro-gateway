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
