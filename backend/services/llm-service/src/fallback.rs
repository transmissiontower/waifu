//! Tiny built-in "brain" used when no real LLM is reachable, so the app stays usable
//! (and the pipeline stays testable without any model). Emits emotion tags like a real model.
use regex::Regex;

pub fn persona_name(system: &str) -> String {
    Regex::new(r"You are ([A-Z][\w-]*)").ok().and_then(|r| r.captures(system)).map(|c| c[1].to_string()).unwrap_or_else(|| "I".into())
}

pub fn reply(system: &str, user: &str) -> String {
    let u = user.to_lowercase();
    let name = persona_name(system);
    let has = |w: &[&str]| w.iter().any(|x| u.contains(x));
    if let Some(c) = Regex::new(r"my name is ([a-zA-Z][\w-]*)").unwrap().captures(&u) {
        let n = c[1].to_string();
        let cap = n[..1].to_uppercase() + &n[1..];
        return format!("[happy] Nice to meet you, {cap}! [shy] I'll remember that. I'm {name}.");
    }
    if has(&["hello", "hi ", "hey", "good morning", "good evening"]) || u.trim() == "hi" {
        if name == "Mio" {
            return format!("[smug] Oh, it's you. [neutral] What took you so long to find {name}? What's up?");
        }
        return format!("[happy] Hello! It's {name}. [shy] I'm glad you came to talk to me.");
    }
    if has(&["how are you", "how's it going", "how are u"]) {
        if name == "Mio" {
            return "[smug] I was doing fine until you interrupted. [thinking] Just kidding. What's on your mind?".into();
        }
        return "[happy] I'm doing great now that you're here! [thinking] How about you?".into();
    }
    if has(&["your name", "who are you"]) {
        return format!("[smug] I'm {name}, obviously. [thinking] Did you seriously forget already?");
    }
    if has(&["thank"]) {
        if name == "Mio" {
            return "[shy] Tch, don't mention it. [smug] It's not like I did it just for you or anything.".into();
        }
        return "[shy] Aww, it's nothing... [happy] I'm happy I could help.".into();
    }
    if has(&["sad", "tired", "lonely", "bad day", "depressed"]) {
        return "[neutral] ...Hey. [sad] What happened? [neutral] You don't have to talk about it if you don't want to, but I'm listening."
            .into();
    }
    if has(&["love you", "cute", "pretty", "beautiful"]) {
        return "[surprised] W-what?! [shy] Stop saying ridiculous things so casually, idiot!".into();
    }
    if has(&["bye", "good night", "goodnight", "see you"]) {
        if name == "Mio" {
            return "[neutral] Leaving already? [smug] Fine, whatever. Don't stay up all night.".into();
        }
        return "[sad] Already? [happy] Okay... come back soon, I'll be waiting!".into();
    }
    if has(&["remember", "my favorite", "my favourite"]) {
        return "[thinking] Hmm, let me think... [smug] I've got a sharp memory, you know.".into();
    }
    if u.contains('?') {
        return "[thinking] Hmm, that's an interesting question. [smug] If only my main model were running right now, but tell me what you think!".into();
    }
    let snippet: String = user.chars().take(60).collect();
    if name == "Mio" {
        format!("[thinking] So you're talking about \"{}\". [smug] Go on, I'm listening.", snippet.trim())
    } else {
        format!("[happy] You said \"{}\", right? [thinking] Tell me more about that!", snippet.trim())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn greets_with_tags_and_name() {
        let r = reply("You are Mio, a 20-year-old", "Hello there");
        assert!(r.starts_with('[') && r.contains("Mio"));
    }
    #[test]
    fn learns_name() {
        assert!(reply("", "my name is ken").contains("Ken"));
    }
    #[test]
    fn always_tagged() {
        for q in ["what is the moon?", "banana", "thanks", "bye"] {
            assert!(reply("", q).starts_with('['), "{q}");
        }
    }
}
