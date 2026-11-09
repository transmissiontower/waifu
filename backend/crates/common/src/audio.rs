//! Small PCM helpers shared by services.

pub fn bytes_to_i16(b: &[u8]) -> Vec<i16> {
    b.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect()
}

pub fn i16_to_bytes(s: &[i16]) -> Vec<u8> {
    let mut v = Vec::with_capacity(s.len() * 2);
    for x in s {
        v.extend_from_slice(&x.to_le_bytes());
    }
    v
}

pub fn f32_to_i16(s: &[f32]) -> Vec<i16> {
    s.iter().map(|x| (x.clamp(-1.0, 1.0) * 32767.0) as i16).collect()
}

pub fn i16_to_f32(s: &[i16]) -> Vec<f32> {
    s.iter().map(|x| *x as f32 / 32768.0).collect()
}

/// Linear resampler (good enough for speech).
pub fn resample(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || input.is_empty() {
        return input.to_vec();
    }
    let ratio = from as f64 / to as f64;
    let n = (input.len() as f64 / ratio) as usize;
    (0..n)
        .map(|i| {
            let pos = i as f64 * ratio;
            let i0 = pos as usize;
            let i1 = (i0 + 1).min(input.len() - 1);
            let t = (pos - i0 as f64) as f32;
            input[i0] * (1.0 - t) + input[i1] * t
        })
        .collect()
}

/// Mouth-openness envelope (0..=255 per frame at `fps`) derived from RMS energy. Used for lip-sync
/// events pushed to external avatars (VTube Studio); the web avatar analyses its own playback.
pub fn mouth_envelope(pcm: &[i16], sample_rate: u32, fps: u32) -> Vec<u8> {
    let win = (sample_rate / fps.max(1)).max(1) as usize;
    pcm.chunks(win)
        .map(|w| {
            let e = (w.iter().map(|x| (*x as f64).powi(2)).sum::<f64>() / w.len() as f64).sqrt() / 32768.0;
            // compress: speech RMS is ~0.02..0.2
            ((e * 6.0).powf(0.7).min(1.0) * 255.0) as u8
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roundtrip() {
        let s = vec![0i16, 1, -1, 32767, -32768];
        assert_eq!(bytes_to_i16(&i16_to_bytes(&s)), s);
    }
    #[test]
    fn resample_halves() {
        let x: Vec<f32> = (0..100).map(|i| i as f32).collect();
        assert_eq!(resample(&x, 32000, 16000).len(), 50);
    }
    #[test]
    fn envelope_silence_vs_loud() {
        let silent = vec![0i16; 1600];
        let loud: Vec<i16> = (0..1600).map(|i| ((i as f32 * 0.3).sin() * 12000.0) as i16).collect();
        assert!(mouth_envelope(&silent, 16000, 50).iter().all(|v| *v == 0));
        assert!(mouth_envelope(&loud, 16000, 50).iter().all(|v| *v > 100));
    }
}
