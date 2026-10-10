pub(crate) fn authorized(supplied: &str, secret: &str) -> bool {
    let expected = format!("Bearer {secret}");
    let mut difference = expected.len() ^ supplied.len();
    for (index, left) in expected.bytes().enumerate() {
        difference |= (left ^ supplied.as_bytes().get(index).copied().unwrap_or(0)) as usize;
    }
    difference == 0
}
