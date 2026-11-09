//! Streaming parser for LLM output: strips `[emotion]` tags (even when split across token
//! boundaries) and cuts the clean text into speakable sentences.

#[derive(Debug, Clone, PartialEq)]
pub enum Piece {
    Emotion(String),
    Sentence(String),
}

pub struct Segmenter {
    allowed: Vec<String>,
    buf: String,      // raw text not yet classified
    sentence: String, // clean text of the sentence being built
    pub clean_total: String,
    first_done: bool,
}

const MIN_SENTENCE: usize = 14;
/// The very first chunk of a reply is cut early (at a comma/clause) so audio starts sooner.
const MIN_FIRST: usize = 8;

fn alias(tag: &str) -> Option<&'static str> {
    Some(match tag {
        "smile" | "smiling" | "laugh" | "laughs" | "laughing" | "giggle" | "joy" | "excited" | "love" | "cheerful" => "happy",
        "cry" | "crying" | "sigh" | "sighs" | "lonely" | "worried" => "sad",
        "mad" | "annoyed" | "pout" | "pouting" | "irritated" | "frown" => "angry",
        "surprise" | "shocked" | "gasp" | "startled" => "surprised",
        "blush" | "blushing" | "embarrassed" | "flustered" | "nervous" => "shy",
        "think" | "hmm" | "curious" | "wonder" | "pondering" => "thinking",
        "smirk" | "smirking" | "teasing" | "proud" | "playful" => "smug",
        "calm" | "relaxed" | "default" => "neutral",
        _ => return None,
    })
}

impl Segmenter {
    pub fn new(allowed: &[String]) -> Self {
        Self {
            allowed: allowed.iter().map(|s| s.to_lowercase()).collect(),
            buf: String::new(),
            sentence: String::new(),
            clean_total: String::new(),
            first_done: false,
        }
    }

    fn resolve(&self, tag: &str) -> Option<String> {
        let t = tag.trim().to_lowercase();
        if self.allowed.contains(&t) {
            return Some(t);
        }
        alias(&t).filter(|a| self.allowed.iter().any(|x| x == a)).map(String::from)
    }

    fn emit_sentence(&mut self, out: &mut Vec<Piece>, force: bool) {
        let s = self.sentence.trim().to_string();
        if s.is_empty() {
            self.sentence.clear();
            return;
        }
        let min = if self.first_done { MIN_SENTENCE } else { MIN_FIRST };
        if !force && s.chars().count() < min {
            return; // keep accumulating short fragments ("Hi!")
        }
        self.clean_total.push_str(&s);
        self.clean_total.push(' ');
        out.push(Piece::Sentence(s));
        self.sentence.clear();
        self.first_done = true;
    }

    pub fn push(&mut self, delta: &str) -> Vec<Piece> {
        self.buf.push_str(delta);
        self.drain(false)
    }

    pub fn finish(&mut self) -> Vec<Piece> {
        self.drain(true)
    }

    fn drain(&mut self, last: bool) -> Vec<Piece> {
        let mut out = vec![];
        loop {
            let early = !self.first_done;
            let Some(i) = self.buf.find(|c: char| {
                matches!(c, '[' | '.' | '!' | '?' | '…' | '\n' | '。' | '！' | '？')
                    || (early && matches!(c, ',' | ';' | ':' | '—' | '、' | '，'))
            }) else {
                // plain text only
                let b = std::mem::take(&mut self.buf);
                self.sentence.push_str(&b);
                break;
            };
            let c = self.buf[i..].chars().next().unwrap();
            if c == '[' {
                match self.buf[i..].find(']') {
                    Some(rel) => {
                        let inner = self.buf[i + 1..i + rel].to_string();
                        let is_tag =
                            inner.len() <= 16 && !inner.is_empty() && inner.chars().all(|c| c.is_alphabetic() || c == ' ' || c == '_');
                        let head: String = self.buf[..i].to_string();
                        self.sentence.push_str(&head);
                        self.buf = self.buf[i + rel + 1..].to_string();
                        if is_tag {
                            if let Some(e) = self.resolve(&inner) {
                                self.emit_sentence(&mut out, true); // tag starts a new sentence
                                out.push(Piece::Emotion(e));
                            }
                            // unknown word-tags are silently stripped (stage directions)
                        } else {
                            self.sentence.push('[');
                            self.sentence.push_str(&inner);
                            self.sentence.push(']');
                        }
                    }
                    None => {
                        if i + 18 < self.buf.len() || last {
                            // not a tag after all
                            let head: String = self.buf[..=i].to_string();
                            self.sentence.push_str(&head);
                            self.buf = self.buf[i + 1..].to_string();
                        } else {
                            // wait for more tokens
                            let head: String = self.buf[..i].to_string();
                            self.sentence.push_str(&head);
                            self.buf = self.buf[i..].to_string();
                            break;
                        }
                    }
                }
            } else {
                // sentence terminator: include it, then require following whitespace (or end when finishing)
                let end = i + c.len_utf8();
                let rest = &self.buf[end..];
                // swallow runs like "..." or "?!"
                let extra: usize =
                    rest.chars().take_while(|c| matches!(c, '.' | '!' | '?' | '…' | '"' | '\'' | ')')).map(|c| c.len_utf8()).sum();
                let end = end + extra;
                let next = self.buf[end..].chars().next();
                let cjk = matches!(c, '。' | '！' | '？') || c == '\n';
                let boundary = cjk || next.map(|n| n.is_whitespace() || n == '[').unwrap_or(true);
                let head: String = self.buf[..end].to_string();
                self.sentence.push_str(&head);
                self.buf = self.buf[end..].to_string();
                if boundary {
                    self.emit_sentence(&mut out, false);
                }
                if next.is_none() {
                    break;
                }
            }
        }
        if last {
            let b = std::mem::take(&mut self.buf);
            self.sentence.push_str(&b);
            self.emit_sentence(&mut out, true);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allowed() -> Vec<String> {
        ["neutral", "happy", "sad", "shy", "surprised", "thinking"].iter().map(|s| s.to_string()).collect()
    }
    fn run(chunks: &[&str]) -> Vec<Piece> {
        let mut s = Segmenter::new(&allowed());
        let mut out = vec![];
        for c in chunks {
            out.extend(s.push(c));
        }
        out.extend(s.finish());
        out
    }
    fn e(x: &str) -> Piece {
        Piece::Emotion(x.into())
    }
    fn s(x: &str) -> Piece {
        Piece::Sentence(x.into())
    }

    #[test]
    fn splits_tags_and_sentences() {
        let p = run(&["[happy] I missed you so much! [shy] D-don't tell anyone I said that."]);
        assert_eq!(p, vec![e("happy"), s("I missed you so much!"), e("shy"), s("D-don't tell anyone I said that.")]);
    }

    #[test]
    fn tag_split_across_tokens() {
        let p = run(&["[ha", "ppy] Hel", "lo there, how are ", "you doing today? ", "[sa", "d] Bye."]);
        assert_eq!(p, vec![e("happy"), s("Hello there,"), s("how are you doing today?"), e("sad"), s("Bye.")]);
    }

    #[test]
    fn unknown_tags_stripped_aliases_mapped() {
        let p = run(&["[giggle] That is really funny to me. [waves hand] Anyway, hello again friend."]);
        assert_eq!(p[0], e("happy"));
        assert!(matches!(&p[1], Piece::Sentence(t) if t.starts_with("That is really funny")));
        assert!(p.iter().all(|x| !matches!(x, Piece::Sentence(t) if t.contains('['))));
    }

    #[test]
    fn short_fragments_merge_and_ellipsis_ok() {
        let p = run(&["Hi! Well... that was unexpected, honestly."]);
        assert_eq!(p, vec![s("Hi! Well..."), s("that was unexpected, honestly.")]);
    }

    #[test]
    fn decimals_do_not_split() {
        let p = run(&["[happy] It costs 3.5 dollars in total, which is fine."]);
        assert_eq!(p, vec![e("happy"), s("It costs 3.5 dollars in total,"), s("which is fine.")]);
    }

    #[test]
    fn sentence_ending_at_token_end_is_emitted_promptly() {
        let mut sg = Segmenter::new(&allowed());
        let got = sg.push("[happy] This is a complete sentence.");
        assert_eq!(got, vec![e("happy"), s("This is a complete sentence.")]);
    }

    #[test]
    fn only_first_chunk_is_cut_at_a_comma() {
        let p = run(&["[happy] Well, that is a long enough second part, with a comma. Then, another one, same story."]);
        assert_eq!(
            p,
            vec![e("happy"), s("Well,"), s("that is a long enough second part, with a comma."), s("Then, another one, same story.")]
        );
    }

    #[test]
    fn untagged_text_still_flows() {
        let p = run(&["No tags at all in this reply"]);
        assert_eq!(p, vec![s("No tags at all in this reply")]);
    }

    #[test]
    fn clean_total_has_no_tags() {
        let mut sg = Segmenter::new(&allowed());
        sg.push("[happy] Good morning to you, sunshine! ");
        sg.finish();
        assert_eq!(sg.clean_total.trim(), "Good morning to you, sunshine!");
    }
}
