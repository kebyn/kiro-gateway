use crate::protocol::internal::Usage;
pub fn merge(a: Option<Usage>, b: Usage) -> Usage {
    let a = a.unwrap_or_else(|| Usage::new(0, 0));
    Usage::new(a.input_tokens.max(b.input_tokens), a.output_tokens.max(b.output_tokens))
}
