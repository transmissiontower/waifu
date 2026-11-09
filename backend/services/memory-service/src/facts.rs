//! Heuristic fact extraction (used when no real LLM is available) and LLM-output parsing.
use regex::Regex;
use serde_json::Value;

pub type Fact = (String, String);

pub fn heuristic_facts(user_text: &str) -> Vec<Fact> {
    let mut out: Vec<Fact> = vec![];
    let t = user_text.trim();
    let grab = |pat: &str| -> Option<String> {
        Regex::new(pat).unwrap().captures(t).map(|c| c[1].trim().trim_end_matches(['.', '!', ',']).to_string())
    };
    if let Some(n) = grab(r"(?i)\b(?:my name is|call me|i'm called|i am called)\s+([A-Za-z][\w'-]*)") {
        out.push(("name".into(), n));
    }
    if let Some(n) = grab(r"(?i)\bi(?:'m| am)\s+(\d{1,2})\s*(?:years? old|yo)\b") {
        out.push(("age".into(), n));
    }
    if let Some(n) =
        grab(r"(?i)\bi (?:live in|am from|'m from|come from)\s+([A-Za-z][A-Za-z .'-]{1,40}?)(?:\s+(?:and|but|where)\b|[.,!?]|$)")
    {
        out.push(("location".into(), n));
    }
    if let Some(n) = grab(r"(?i)\bi work (?:as|at|in|for)\s+(?:an? )?([A-Za-z][A-Za-z .'-]{1,40}?)(?:\s+(?:and|but)\b|[.,!?]|$)") {
        out.push(("job".into(), n));
    }
    if let Some(n) = grab(r"(?i)\bmy birthday is\s+(?:on )?([A-Za-z0-9 ,]+?)(?:[.!?]|$)") {
        out.push(("birthday".into(), n));
    }
    if let Some(c) =
        Regex::new(r"(?i)\bmy (dog|cat|pet|rabbit|hamster|bird)(?:'s name)? (?:is named|is called|was named|is)\s+([A-Za-z][\w'-]*)")
            .unwrap()
            .captures(t)
    {
        out.push((format!("{}_name", c[1].to_lowercase()), c[2].to_string()));
    }
    if let Some(c) = Regex::new(r"(?i)\bmy fav(?:ou?rite)? ([a-z ]{2,20}?) is\s+([^.!?,]{1,40})").unwrap().captures(t) {
        out.push((format!("favorite_{}", c[1].trim().replace(' ', "_").to_lowercase()), c[2].trim().to_string()));
    }
    if let Some(n) = grab(r"(?i)\bi (?:really )?(?:like|love|enjoy|adore)\s+([^.!?,]{2,50})") {
        out.push(("likes".into(), n));
    }
    if let Some(n) = grab(r"(?i)\bi (?:really )?(?:hate|dislike|can't stand|don't like)\s+([^.!?,]{2,50})") {
        out.push(("dislikes".into(), n));
    }
    out
}

/// Parse `[{"key":"..","value":".."}]` (possibly wrapped in prose / code fences) from LLM output.
pub fn parse_llm_facts(text: &str) -> Vec<Fact> {
    let (Some(a), Some(b)) = (text.find('['), text.rfind(']')) else { return vec![] };
    if b <= a {
        return vec![];
    }
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(&text[a..=b]) else { return vec![] };
    items
        .iter()
        .filter_map(|i| {
            let k = i["key"].as_str()?.trim().to_lowercase().replace(' ', "_");
            let v = i["value"].as_str().map(String::from).or_else(|| i["value"].as_i64().map(|n| n.to_string()))?;
            (!k.is_empty() && !v.trim().is_empty() && k.len() < 40 && v.len() < 200).then(|| (k, v.trim().to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn has(f: &[Fact], k: &str, v: &str) -> bool {
        f.iter().any(|(a, b)| a == k && b.eq_ignore_ascii_case(v))
    }
    #[test]
    fn extracts_basics() {
        let f = heuristic_facts("Hi, my name is Ken. I'm 27 years old and I live in Berlin.");
        assert!(has(&f, "name", "Ken"), "{f:?}");
        assert!(has(&f, "age", "27"), "{f:?}");
        assert!(has(&f, "location", "Berlin"), "{f:?}");
    }
    #[test]
    fn extracts_pets_likes() {
        let f = heuristic_facts("My cat is named Tofu and I love hiking in the mountains.");
        assert!(has(&f, "cat_name", "Tofu"), "{f:?}");
        assert!(has(&f, "likes", "hiking in the mountains"), "{f:?}");
    }
    #[test]
    fn parses_fenced_llm_json() {
        let f = parse_llm_facts("Sure!\n```json\n[{\"key\":\"Favorite Food\",\"value\":\"ramen\"}]\n```");
        assert_eq!(f, vec![("favorite_food".to_string(), "ramen".to_string())]);
        assert!(parse_llm_facts("no json").is_empty());
    }
}
