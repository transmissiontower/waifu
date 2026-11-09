//! Local embeddings. The default `hash` embedder is a deterministic feature-hashing model
//! (word unigrams/bigrams + char trigrams) - zero downloads, instant, decent for keyword-ish recall.
//! `embedder: llm` uses the llm-service `Embed` RPC (e.g. Ollama `nomic-embed-text`) for semantic recall.

const DIM: usize = 384;
const STOP: &[&str] = &[
    "the", "a", "an", "and", "or", "of", "to", "is", "it", "i", "you", "my", "me", "in", "on", "for", "that", "this", "was", "are", "be",
    "so", "but",
];

fn fnv(s: &str, seed: u64) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325 ^ seed;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn add(v: &mut [f32], feat: &str, w: f32) {
    let h = fnv(feat, 0);
    let idx = (h % DIM as u64) as usize;
    let sign = if (fnv(feat, 7) & 1) == 0 { 1.0 } else { -1.0 };
    v[idx] += sign * w;
}

pub fn hash_embed(text: &str) -> Vec<f32> {
    let lower = text.to_lowercase();
    let words: Vec<&str> = lower.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty() && !STOP.contains(w)).collect();
    let mut v = vec![0f32; DIM];
    for (i, w) in words.iter().enumerate() {
        add(&mut v, w, 1.0);
        if let Some(n) = words.get(i + 1) {
            add(&mut v, &format!("{w}_{n}"), 0.7);
        }
        let chars: Vec<char> = w.chars().collect();
        if chars.len() > 3 {
            for t in chars.windows(3) {
                add(&mut v, &format!("#{}", t.iter().collect::<String>()), 0.3);
            }
        }
    }
    normalize(&mut v);
    v
}

pub fn normalize(v: &mut [f32]) {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        v.iter_mut().for_each(|x| *x /= n);
    }
}

pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
}

pub fn to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}
pub fn from_blob(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn related_text_scores_higher() {
        let q = hash_embed("what is my cat called");
        let a = hash_embed("User: my cat Tofu loves sleeping on the keyboard");
        let b = hash_embed("User: the weather in Berlin was rainy yesterday");
        assert!(cosine(&q, &a) > cosine(&q, &b));
    }
    #[test]
    fn blob_roundtrip() {
        let v = hash_embed("hello world");
        assert_eq!(from_blob(&to_blob(&v)), v);
    }
}
