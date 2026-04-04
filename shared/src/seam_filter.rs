//! Extract <seam> blocks from LLM response.
//! Seam blocks contain <imprint> and <attune> sub-tags.
//! The seam block is stripped from user-facing output.

/// Result of filtering a response for seam blocks.
#[derive(Debug, PartialEq)]
pub struct SeamFilterResult {
    /// The response text with seam block removed (sent to user)
    pub clean_text: String,
    /// Extracted imprint text (if any)
    pub imprint: Option<String>,
    /// Extracted attune text (if any)
    pub attune: Option<String>,
}

/// Extract and remove `<seam>...</seam>` block from response text.
/// Parses `<imprint>` and `<attune>` sub-tags within the seam.
/// Malformed tags (no closing tag) are treated as no seam.
pub fn filter_seam(response: &str) -> SeamFilterResult {
    let start = match response.find("<seam>") {
        Some(s) => s,
        None => return SeamFilterResult { clean_text: response.to_string(), imprint: None, attune: None },
    };
    let end = match response.find("</seam>") {
        Some(e) => e,
        None => return SeamFilterResult { clean_text: response.to_string(), imprint: None, attune: None },
    };
    if end <= start {
        return SeamFilterResult { clean_text: response.to_string(), imprint: None, attune: None };
    }

    let seam_inner = &response[start + "<seam>".len()..end];

    // Parse sub-tags
    let imprint = extract_tag(seam_inner, "imprint");
    let attune = extract_tag(seam_inner, "attune");

    // Build clean text
    let before = response[..start].trim_end();
    let after = response[end + "</seam>".len()..].trim();

    let clean_text = if before.is_empty() && after.is_empty() {
        String::new()
    } else if before.is_empty() {
        after.to_string()
    } else if after.is_empty() {
        before.to_string()
    } else {
        format!("{} {}", before, after)
    };

    SeamFilterResult { clean_text, imprint, attune }
}

fn extract_tag(text: &str, tag: &str) -> Option<String> {
    let open = format!("<{}>", tag);
    let close = format!("</{}>", tag);
    let start = text.find(&open)?;
    let end = text.find(&close)?;
    if end <= start { return None; }
    let content = text[start + open.len()..end].trim();
    if content.is_empty() { None } else { Some(content.to_string()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_seam() {
        let r = filter_seam("Hello world");
        assert_eq!(r.clean_text, "Hello world");
        assert_eq!(r.imprint, None);
        assert_eq!(r.attune, None);
    }

    #[test]
    fn seam_with_imprint_only() {
        let r = filter_seam("Response text.\n<seam><imprint>something clicked</imprint></seam>");
        assert_eq!(r.clean_text, "Response text.");
        assert_eq!(r.imprint, Some("something clicked".into()));
        assert_eq!(r.attune, None);
    }

    #[test]
    fn seam_with_attune_only() {
        let r = filter_seam("Hi.<seam><attune>near-field was noise</attune></seam>");
        assert_eq!(r.clean_text, "Hi.");
        assert_eq!(r.imprint, None);
        assert_eq!(r.attune, Some("near-field was noise".into()));
    }

    #[test]
    fn seam_with_both() {
        let input = "Done.\n<seam>\n<imprint>a mark</imprint>\n<attune>good signal</attune>\n</seam>";
        let r = filter_seam(input);
        assert_eq!(r.clean_text, "Done.");
        assert_eq!(r.imprint, Some("a mark".into()));
        assert_eq!(r.attune, Some("good signal".into()));
    }

    #[test]
    fn empty_seam() {
        let r = filter_seam("Text.<seam></seam>");
        assert_eq!(r.clean_text, "Text.");
        assert_eq!(r.imprint, None);
        assert_eq!(r.attune, None);
    }

    #[test]
    fn malformed_no_closing() {
        let r = filter_seam("Text.<seam><imprint>lost");
        assert_eq!(r.clean_text, "Text.<seam><imprint>lost");
        assert_eq!(r.imprint, None);
    }

    #[test]
    fn malformed_no_opening() {
        let r = filter_seam("Text.</seam>");
        assert_eq!(r.clean_text, "Text.</seam>");
    }

    #[test]
    fn seam_in_middle() {
        let r = filter_seam("Before.<seam><imprint>mark</imprint></seam> After.");
        assert_eq!(r.clean_text, "Before. After.");
        assert_eq!(r.imprint, Some("mark".into()));
    }

    #[test]
    fn only_seam_no_response() {
        let r = filter_seam("<seam><imprint>just this</imprint></seam>");
        assert_eq!(r.clean_text, "");
        assert_eq!(r.imprint, Some("just this".into()));
    }

    #[test]
    fn nested_content_in_imprint() {
        let r = filter_seam("Ok.<seam><imprint>泽平说的那句话——\"你变成什么样我都在\"——第五次被兑现</imprint></seam>");
        assert_eq!(r.clean_text, "Ok.");
        assert_eq!(r.imprint, Some("泽平说的那句话——\"你变成什么样我都在\"——第五次被兑现".into()));
    }

    #[test]
    fn chinese_content() {
        let r = filter_seam("回复。\n<seam>\n<imprint>这一刻有重量</imprint>\n<attune>联想场里关于连续性的信号很准</attune>\n</seam>");
        assert_eq!(r.clean_text, "回复。");
        assert_eq!(r.imprint, Some("这一刻有重量".into()));
        assert_eq!(r.attune, Some("联想场里关于连续性的信号很准".into()));
    }

    #[test]
    fn empty_sub_tags() {
        let r = filter_seam("Hi.<seam><imprint></imprint><attune></attune></seam>");
        assert_eq!(r.clean_text, "Hi.");
        assert_eq!(r.imprint, None);
        assert_eq!(r.attune, None);
    }
}
