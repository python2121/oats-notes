//! The capture conversion path without a device: integer sample formats through
//! `mix_to_mono` and the resampler, mixdown of 3+ channels, and sample-at-a-time streaming.

use std::f64::consts::PI;

use oats_audio::mix_to_mono;
use oats_audio::resample::Resampler;

fn sine(freq: f64, rate: u32, len: usize, amp: f64) -> Vec<f64> {
    (0..len)
        .map(|i| amp * (2.0 * PI * freq * i as f64 / f64::from(rate)).sin())
        .collect()
}

fn resample(input: &[f32], in_rate: u32) -> Vec<f32> {
    let mut r = Resampler::new(in_rate, 16_000);
    let mut out = Vec::new();
    r.process(input, &mut out);
    r.flush(&mut out);
    out
}

fn max_diff(a: &[f32], b: &[f32]) -> f32 {
    assert_eq!(a.len(), b.len());
    a.iter()
        .zip(b)
        .map(|(x, y)| (x - y).abs())
        .fold(0.0, f32::max)
}

/// The same stereo 48 kHz sine as f32, i16, u16 and i32 frames: after mixdown and resampling
/// all four agree with the f32 path to within the format's quantisation.
#[test]
fn integer_formats_match_the_float_path() {
    let rate = 48_000;
    let left = sine(440.0, rate, 9_600, 0.5);
    let right = sine(660.0, rate, 9_600, 0.25);
    let interleave = |f: &dyn Fn(f64) -> f32| -> Vec<f32> {
        left.iter()
            .zip(&right)
            .flat_map(|(l, r)| [f(*l), f(*r)])
            .collect()
    };
    let f32_frames = interleave(&|x| x as f32);
    let mut mono_f32 = Vec::new();
    mix_to_mono(&f32_frames, 2, &mut mono_f32);
    let want = resample(&mono_f32, rate);

    let i16_frames: Vec<i16> = left
        .iter()
        .zip(&right)
        .flat_map(|(l, r)| [(l * 32_768.0).round() as i16, (r * 32_768.0).round() as i16])
        .collect();
    let u16_frames: Vec<u16> = i16_frames
        .iter()
        .map(|s| (i32::from(*s) + 32_768) as u16)
        .collect();
    let i32_frames: Vec<i32> = left
        .iter()
        .zip(&right)
        .flat_map(|(l, r)| {
            [
                (l * 2_147_483_648.0).round() as i32,
                (r * 2_147_483_648.0).round() as i32,
            ]
        })
        .collect();

    let mut m = Vec::new();
    mix_to_mono(&i16_frames, 2, &mut m);
    assert_eq!(m.len(), mono_f32.len());
    assert!(max_diff(&m, &mono_f32) < 2.0 / 32_768.0);
    assert!(max_diff(&resample(&m, rate), &want) < 4.0 / 32_768.0);

    let mut m = Vec::new();
    mix_to_mono(&u16_frames, 2, &mut m);
    assert!(
        max_diff(&m, &mono_f32) < 2.0 / 32_768.0,
        "u16 is offset binary"
    );
    assert!(max_diff(&resample(&m, rate), &want) < 4.0 / 32_768.0);

    let mut m = Vec::new();
    mix_to_mono(&i32_frames, 2, &mut m);
    assert!(max_diff(&m, &mono_f32) < 1e-6);
    assert!(max_diff(&resample(&m, rate), &want) < 1e-5);

    // full scale maps to ±1 in every format
    let mut m = Vec::new();
    mix_to_mono(&[i16::MIN, i16::MAX], 1, &mut m);
    mix_to_mono(&[u16::MIN, u16::MAX], 1, &mut m);
    mix_to_mono(&[i32::MIN, i32::MAX], 1, &mut m);
    for pair in m.chunks(2) {
        assert!(
            (pair[0] + 1.0).abs() < 1e-4 && (pair[1] - 1.0).abs() < 1e-4,
            "{pair:?}"
        );
    }
}

#[test]
fn mixdown_of_three_and_more_channels_averages_each_frame() {
    let mut out = Vec::new();
    mix_to_mono(&[0.3f32, 0.6, 0.9, -0.3, -0.6, -0.9], 3, &mut out);
    assert_eq!(out.len(), 2);
    assert!(
        (out[0] - 0.6).abs() < 1e-6 && (out[1] + 0.6).abs() < 1e-6,
        "{out:?}"
    );

    // 5.1: one loud channel among six
    let mut out = Vec::new();
    let frame = [0.0f32, 0.0, 0.6, 0.0, 0.0, 0.0];
    mix_to_mono(&frame.repeat(4), 6, &mut out);
    assert_eq!(out.len(), 4);
    assert!(out.iter().all(|s| (s - 0.1).abs() < 1e-6), "{out:?}");

    // 4-channel i16: integer conversion happens before averaging (no overflow)
    let mut out = Vec::new();
    mix_to_mono(&[i16::MAX; 8], 4, &mut out);
    assert!(out.iter().all(|s| (s - 1.0).abs() < 1e-3), "{out:?}");

    // a trailing partial frame is dropped, not mixed as if the missing channels were silent
    let mut out = Vec::new();
    mix_to_mono(&[1.0f32, 1.0, 1.0, 0.5, 0.5], 3, &mut out);
    assert_eq!(out, vec![1.0]);

    // zero channels is treated as mono; appending keeps what was there
    let mut out = vec![9.0];
    mix_to_mono(&[0.25f32, 0.5], 0, &mut out);
    assert_eq!(out, vec![9.0, 0.25, 0.5]);
}

/// Feeding one sample per call produces bit-for-bit the same output as one big call.
#[test]
fn one_sample_at_a_time_equals_one_big_chunk() {
    for in_rate in [48_000u32, 44_100, 22_050, 8_000, 96_000, 11_025] {
        let input: Vec<f32> = sine(997.0, in_rate, in_rate as usize / 4, 0.6)
            .into_iter()
            .zip(sine(3_100.0, in_rate, in_rate as usize / 4, 0.2))
            .map(|(a, b)| (a + b) as f32)
            .collect();
        let whole = resample(&input, in_rate);

        let mut r = Resampler::new(in_rate, 16_000);
        let mut streamed = Vec::new();
        let mut lengths = Vec::new();
        for s in &input {
            r.process(std::slice::from_ref(s), &mut streamed);
            lengths.push(streamed.len());
        }
        r.flush(&mut streamed);
        assert_eq!(streamed.len(), whole.len(), "{in_rate} Hz");
        assert!(
            streamed
                .iter()
                .zip(&whole)
                .all(|(a, b)| a.to_bits() == b.to_bits()),
            "{in_rate} Hz: streamed output differs"
        );
        // output appears progressively (never more than one input's worth at once when
        // downsampling; at most two per input when upsampling 8 kHz → 16 kHz)
        let max_step = lengths.windows(2).map(|w| w[1] - w[0]).max().unwrap_or(0);
        let bound = (16_000usize).div_ceil(in_rate as usize);
        assert!(
            max_step <= bound,
            "{in_rate} Hz: {max_step} outputs from one input"
        );
        // and the output length is exactly ceil(len · 16000 / rate)
        let expected = (input.len() as u64 * 16_000).div_ceil(u64::from(in_rate)) as usize;
        assert_eq!(whole.len(), expected, "{in_rate} Hz");
    }
}

#[test]
fn flush_resets_so_a_second_stream_matches_the_first() {
    let input: Vec<f32> = sine(440.0, 44_100, 4_410, 0.5)
        .into_iter()
        .map(|x| x as f32)
        .collect();
    let mut r = Resampler::new(44_100, 16_000);
    let mut first = Vec::new();
    r.process(&input, &mut first);
    r.flush(&mut first);
    let mut second = Vec::new();
    r.process(&input, &mut second);
    r.flush(&mut second);
    assert_eq!(first, second);
    // flushing an empty stream emits nothing
    let mut empty = Vec::new();
    r.flush(&mut empty);
    assert!(empty.is_empty());
    let mut fresh = Resampler::new(48_000, 16_000);
    fresh.flush(&mut empty);
    assert!(empty.is_empty());
}

#[test]
fn silence_and_dc_stay_silence_and_dc() {
    for rate in [48_000u32, 44_100, 8_000] {
        let out = resample(&vec![0.0; rate as usize / 10], rate);
        assert!(out.iter().all(|s| *s == 0.0), "{rate}");
        // DC: unit gain per phase (away from the zero-padded edges)
        let out = resample(&vec![0.25; rate as usize], rate);
        let mid = &out[100..out.len() - 100];
        assert!(mid.iter().all(|s| (s - 0.25).abs() < 1e-4), "{rate}");
    }
}

#[test]
fn degenerate_rates_pass_through() {
    for (a, b) in [(16_000, 16_000), (0, 16_000), (48_000, 0)] {
        let mut r = Resampler::new(a, b);
        assert!(r.is_passthrough(), "{a} → {b}");
        let mut out = Vec::new();
        r.process(&[0.5, -0.5], &mut out);
        r.flush(&mut out);
        assert_eq!(out, vec![0.5, -0.5]);
    }
    assert!(!Resampler::new(44_101, 16_000).is_passthrough());
}
