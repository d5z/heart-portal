/// Real-time streaming filter for `<seam>...</seam>` tags.
///
/// Prevents seam content from leaking through SSE deltas.
/// Three-state machine: Normal → MaybeSeam → InSeam.

const OPEN_TAG: &str = "<seam>";
const CLOSE_TAG: &str = "</seam>";

#[derive(Debug)]
enum FilterState {
    Normal,
    MaybeSeam,  // buffer might be start of <seam>
    InSeam,     // inside <seam>...</seam>, looking for close tag
}

#[derive(Debug)]
pub struct SeamStreamFilter {
    state: FilterState,
    buffer: String,
}

impl SeamStreamFilter {
    pub fn new() -> Self {
        Self {
            state: FilterState::Normal,
            buffer: String::new(),
        }
    }

    pub fn feed(&mut self, delta: &str) -> String {
        let mut output = String::new();
        self.buffer.push_str(delta);

        loop {
            match self.state {
                FilterState::Normal => {
                    // Look for "<seam>" or a prefix of it
                    if let Some(pos) = self.buffer.find(OPEN_TAG) {
                        // Full open tag found — emit everything before it, enter InSeam
                        output.push_str(&self.buffer[..pos]);
                        self.buffer = self.buffer[pos + OPEN_TAG.len()..].to_string();
                        self.state = FilterState::InSeam;
                        continue;
                    }
                    // Check if tail of buffer could be a prefix of "<seam>"
                    let safe = self.find_safe_end(&self.buffer.clone(), OPEN_TAG);
                    if safe < self.buffer.len() {
                        output.push_str(&self.buffer[..safe]);
                        self.buffer = self.buffer[safe..].to_string();
                        self.state = FilterState::MaybeSeam;
                    } else {
                        output.push_str(&self.buffer);
                        self.buffer.clear();
                    }
                    break;
                }
                FilterState::MaybeSeam => {
                    // Check if buffer matches a prefix of OPEN_TAG
                    if self.buffer.len() >= OPEN_TAG.len() {
                        if self.buffer.starts_with(OPEN_TAG) {
                            // Confirmed — drop the tag, enter InSeam
                            self.buffer = self.buffer[OPEN_TAG.len()..].to_string();
                            self.state = FilterState::InSeam;
                            continue;
                        } else {
                            // Not a match — release buffer back to Normal
                            self.state = FilterState::Normal;
                            continue;
                        }
                    }
                    // Still a valid prefix?
                    if OPEN_TAG.starts_with(&self.buffer) {
                        // Still ambiguous, keep buffering
                        break;
                    } else {
                        // Not a prefix — release buffer back to Normal
                        self.state = FilterState::Normal;
                        continue;
                    }
                }
                FilterState::InSeam => {
                    // Look for "</seam>"
                    if let Some(pos) = self.buffer.find(CLOSE_TAG) {
                        // Found close tag — discard everything up to and including it
                        self.buffer = self.buffer[pos + CLOSE_TAG.len()..].to_string();
                        self.state = FilterState::Normal;
                        continue;
                    }
                    // Check if tail could be prefix of "</seam>"
                    let safe_discard = self.find_safe_end(&self.buffer.clone(), CLOSE_TAG);
                    // Discard everything up to safe_discard, keep the rest
                    self.buffer = self.buffer[safe_discard..].to_string();
                    break;
                }
            }
        }

        output
    }

    pub fn flush(&mut self) -> String {
        match self.state {
            FilterState::Normal => {
                let out = self.buffer.clone();
                self.buffer.clear();
                out
            }
            FilterState::MaybeSeam => {
                // Never confirmed as seam — release buffer
                let out = self.buffer.clone();
                self.buffer.clear();
                self.state = FilterState::Normal;
                out
            }
            FilterState::InSeam => {
                // Incomplete seam — discard defensively
                self.buffer.clear();
                self.state = FilterState::Normal;
                String::new()
            }
        }
    }

    /// Find the largest safe prefix length where no suffix of `text[..n]`
    /// is a prefix of `tag`. Returns the safe end index.
    fn find_safe_end(&self, text: &str, tag: &str) -> usize {
        let bytes = text.as_bytes();
        let tag_bytes = tag.as_bytes();

        // Check each possible suffix of text against prefix of tag
        for start in (0..bytes.len()).rev() {
            let suffix = &bytes[start..];
            let check_len = suffix.len().min(tag_bytes.len());
            if suffix[..check_len] == tag_bytes[..check_len] {
                return start;
            }
        }
        bytes.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_all(filter: &mut SeamStreamFilter, deltas: &[&str]) -> String {
        let mut out = String::new();
        for d in deltas {
            out.push_str(&filter.feed(d));
        }
        out.push_str(&filter.flush());
        out
    }

    #[test]
    fn test_normal_text_passes_through() {
        let mut f = SeamStreamFilter::new();
        assert_eq!(feed_all(&mut f, &["hello ", "world"]), "hello world");
    }

    #[test]
    fn test_single_delta_complete_seam() {
        let mut f = SeamStreamFilter::new();
        assert_eq!(feed_all(&mut f, &["before<seam>secret</seam>after"]), "beforeafter");
    }

    #[test]
    fn test_seam_across_deltas() {
        let mut f = SeamStreamFilter::new();
        assert_eq!(feed_all(&mut f, &["be", "fore<se", "am>sec", "ret</se", "am>af", "ter"]), "beforeafter");
    }

    #[test]
    fn test_seam_after_normal_text() {
        let mut f = SeamStreamFilter::new();
        assert_eq!(feed_all(&mut f, &["hello<seam>hidden</seam>"]), "hello");
    }

    #[test]
    fn test_content_after_close_seam() {
        let mut f = SeamStreamFilter::new();
        assert_eq!(feed_all(&mut f, &["<seam>hidden</seam>visible"]), "visible");
    }

    #[test]
    fn test_false_prefix_recovery() {
        let mut f = SeamStreamFilter::new();
        // "<se" looks like start of <seam> but then "rious" makes it not
        assert_eq!(feed_all(&mut f, &["hello <se", "rious text"]), "hello <serious text");
    }

    #[test]
    fn test_chinese_content() {
        let mut f = SeamStreamFilter::new();
        assert_eq!(feed_all(&mut f, &["你好<seam>秘密</seam>世界"]), "你好世界");
    }

    #[test]
    fn test_flush_normal() {
        let mut f = SeamStreamFilter::new();
        let out = f.feed("hello");
        assert_eq!(out, "hello");
        assert_eq!(f.flush(), "");
    }

    #[test]
    fn test_flush_maybe_seam() {
        let mut f = SeamStreamFilter::new();
        let out = f.feed("text<sea");
        assert_eq!(out, "text");
        // flush releases buffer since it never became a real seam
        assert_eq!(f.flush(), "<sea");
    }

    #[test]
    fn test_flush_in_seam() {
        let mut f = SeamStreamFilter::new();
        let out = f.feed("text<seam>incomplete");
        assert_eq!(out, "text");
        // flush discards incomplete seam content
        assert_eq!(f.flush(), "");
    }

    #[test]
    fn test_less_than_in_normal_text() {
        let mut f = SeamStreamFilter::new();
        assert_eq!(feed_all(&mut f, &["1 < 2 and 3 > 1"]), "1 < 2 and 3 > 1");
    }

    #[test]
    fn test_angle_bracket_not_seam() {
        let mut f = SeamStreamFilter::new();
        assert_eq!(feed_all(&mut f, &["<div>hello</div>"]), "<div>hello</div>");
    }

    #[test]
    fn test_consecutive_seams() {
        let mut f = SeamStreamFilter::new();
        assert_eq!(feed_all(&mut f, &["a<seam>x</seam>b<seam>y</seam>c"]), "abc");
    }

    #[test]
    fn test_seam_split_at_every_char() {
        let mut f = SeamStreamFilter::new();
        assert_eq!(
            feed_all(&mut f, &["<", "s", "e", "a", "m", ">", "hidden", "<", "/", "s", "e", "a", "m", ">", "ok"]),
            "ok"
        );
    }

    #[test]
    fn test_multiple_false_starts() {
        let mut f = SeamStreamFilter::new();
        assert_eq!(feed_all(&mut f, &["<s not seam <seam>hidden</seam>"]), "<s not seam ");
    }
}
