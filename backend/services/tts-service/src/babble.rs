//! Last-resort voice: formant "babble" (think Animal Crossing). Keeps the whole audio/lip-sync
//! pipeline working when no neural voice model is installed. Deterministic and dependency-free.
use std::f32::consts::PI;

pub const SAMPLE_RATE: u32 = 22050;

fn formants(c: char) -> (f32, f32) {
    match c {
        'a' => (800.0, 1200.0),
        'e' => (500.0, 1900.0),
        'i' => (300.0, 2300.0),
        'o' => (500.0, 900.0),
        'u' => (350.0, 800.0),
        _ => (450.0 + (c as u32 % 7) as f32 * 60.0, 1500.0 + (c as u32 % 5) as f32 * 200.0),
    }
}

pub fn synth(text: &str, speed: f32) -> Vec<f32> {
    let speed = if speed <= 0.0 { 1.0 } else { speed };
    let sr = SAMPLE_RATE as f32;
    let mut out: Vec<f32> = vec![];
    let mut phase = 0f32;
    for (i, ch) in text.to_lowercase().chars().enumerate() {
        let (dur, gap) = if ch.is_alphabetic() {
            (0.075 / speed, 0.0)
        } else if ch == ' ' {
            (0.0, 0.05 / speed)
        } else if matches!(ch, '.' | '!' | '?' | ',' | ';' | ':') {
            (0.0, 0.22 / speed)
        } else {
            (0.0, 0.0)
        };
        if gap > 0.0 {
            out.extend(std::iter::repeat_n(0.0, (gap * sr) as usize));
            continue;
        }
        if dur == 0.0 {
            continue;
        }
        let (f1, f2) = formants(ch);
        let pitch = 190.0 + ((i * 37) % 9) as f32 * 14.0;
        let n = (dur * sr) as usize;
        for k in 0..n {
            let t = k as f32 / n as f32;
            let env = (PI * t).sin().powf(0.6);
            phase += 2.0 * PI * pitch / sr;
            let tt = k as f32 / sr;
            let s = 0.6 * phase.sin()
                + 0.25 * (2.0 * PI * f1 * tt).sin() * (phase * 0.5).sin().abs()
                + 0.15 * (2.0 * PI * f2 * tt).sin() * (phase * 0.5).sin().abs();
            out.push(s * env * 0.35);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn length_scales_with_text_and_speed() {
        let a = synth("hello there", 1.0);
        let b = synth("hello there, how are you today?", 1.0);
        let fast = synth("hello there", 2.0);
        assert!(b.len() > a.len() && fast.len() < a.len());
        assert!(a.iter().any(|x| x.abs() > 0.05));
        assert!(a.iter().all(|x| x.abs() <= 1.0));
    }
}
