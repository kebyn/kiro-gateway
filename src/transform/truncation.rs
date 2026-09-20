/// Filters only the XML-like markers emitted by Kiro when a tool call leaks
/// into a text channel. Ordinary markup and comparison expressions remain
/// untouched.
#[derive(Clone, Debug, Default)]
pub struct XmlLeakFilter {
    pending: String,
}

const KIRO_MARKERS: &[&str] = &[
    "<tool_use>",
    "</tool_use>",
    "<tool_result>",
    "</tool_result>",
    "<tool_call>",
    "</tool_call>",
];

impl XmlLeakFilter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, input: &str) -> String {
        self.pending.push_str(input);
        self.drain(false)
    }

    /// Flushes a partial, non-marker fragment at end of stream.
    pub fn finish(&mut self) -> String {
        self.drain(true)
    }

    fn drain(&mut self, final_chunk: bool) -> String {
        let mut output = String::with_capacity(self.pending.len());
        loop {
            if self.pending.is_empty() {
                break;
            }
            if let Some(marker) =
                KIRO_MARKERS.iter().find(|marker| self.pending.starts_with(**marker))
            {
                self.pending.drain(..marker.len());
                continue;
            }
            if !final_chunk
                && KIRO_MARKERS.iter().any(|marker| marker.starts_with(self.pending.as_str()))
            {
                break;
            }
            let Some(position) = self.pending.find('<') else {
                output.push_str(&self.pending);
                self.pending.clear();
                break;
            };
            if position > 0 {
                output.push_str(&self.pending[..position]);
                self.pending.drain(..position);
                continue;
            }
            // A '<' which is not the beginning of a known marker is ordinary
            // text (for example `a < b` or `<custom-tag>`).
            output.push('<');
            self.pending.drain(..1);
        }
        output
    }
}

#[cfg(test)]
mod tests {
    use super::XmlLeakFilter;

    #[test]
    fn filters_only_known_markers_across_chunks() {
        let mut filter = XmlLeakFilter::new();
        assert_eq!(filter.push("before <tool"), "before ");
        assert_eq!(filter.push("_use>{\"x\":1}</tool_use> after"), "{\"x\":1} after");
    }

    #[test]
    fn preserves_markup_comparisons_and_code_examples() {
        let mut filter = XmlLeakFilter::new();
        let text = "a < b and c > d <custom-tag>literal</custom-tag> `<tool>`";
        assert_eq!(filter.push(text), text);
        assert_eq!(filter.finish(), "");
    }

    #[test]
    fn flushes_unknown_partial_text() {
        let mut filter = XmlLeakFilter::new();
        assert_eq!(filter.push("hello <tool"), "hello ");
        assert_eq!(filter.finish(), "<tool");
    }
}
