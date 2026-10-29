//! Shared by the examples (`#[path = "common/wav.rs"] mod wav;`).

/// Minimal RIFF/WAVE reader for PCM 16-bit mono at 16 kHz (what `say` writes).
pub fn read_wav(bytes: &[u8]) -> Result<Vec<f32>, String> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("not a RIFF/WAVE file".into());
    }
    let mut pos = 12;
    let mut format: Option<(u16, u16, u32, u16)> = None;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32::from_le_bytes([
            bytes[pos + 4],
            bytes[pos + 5],
            bytes[pos + 6],
            bytes[pos + 7],
        ]) as usize;
        let body = &bytes[pos + 8..(pos + 8 + size).min(bytes.len())];
        match id {
            b"fmt " => {
                let u16_at = |i: usize| u16::from_le_bytes([body[i], body[i + 1]]);
                let u32_at =
                    |i: usize| u32::from_le_bytes([body[i], body[i + 1], body[i + 2], body[i + 3]]);
                format = Some((u16_at(0), u16_at(2), u32_at(4), u16_at(14)));
            }
            b"data" => {
                let (tag, channels, rate, bits) = format.ok_or("data before fmt")?;
                if tag != 1 || channels != 1 || rate != 16_000 || bits != 16 {
                    return Err(format!(
                        "need PCM 16-bit mono 16 kHz, got tag {tag}, {channels} ch, {rate} Hz, {bits} bit"
                    ));
                }
                return Ok(body
                    .chunks_exact(2)
                    .map(|c| f32::from(i16::from_le_bytes([c[0], c[1]])) / 32768.0)
                    .collect());
            }
            _ => {}
        }
        pos += 8 + size + (size & 1);
    }
    Err("no data chunk".into())
}
