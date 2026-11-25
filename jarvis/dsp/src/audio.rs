//! Обработка звука для динамика: подъём частоты до 48 кГц, ограничитель громкости, мягкий потолок.

use std::collections::VecDeque;

/// Частота, которую плеер всегда отдаёт в ALSA.
pub const OUT_RATE: usize = 48000;

const UP_TAPS: usize = 24; // коэффициентов на фазу: подавление зеркал около −70 дБ

/// Голос моделей приходит в 16 или 24 кГц, а dmix колонки работает на 48 кГц. Пересчёт ALSA (линейный)
/// даёт «звон» зеркальных призвуков, поэтому поднимаем сами: вставка нулей и полифазный FIR (окно Блэкмана).
pub struct Upsampler {
    l: usize,
    phase: Vec<[f32; UP_TAPS]>,
    buf: Vec<f32>,
}

impl Upsampler {
    /// Подъём частоты в l раз (l ≥ 2).
    pub fn new(l: usize) -> Self {
        let n = l * UP_TAPS;
        let fc = 0.5 / l as f64 * 0.9;
        let mid = (n - 1) as f64 / 2.0;
        let pi = std::f64::consts::PI;
        let h: Vec<f64> = (0..n)
            .map(|k| {
                let t = k as f64 - mid;
                let s = if t != 0.0 { (2.0 * pi * fc * t).sin() / (pi * t) } else { 2.0 * fc };
                let w = 0.42 - 0.5 * (2.0 * pi * k as f64 / (n - 1) as f64).cos() + 0.08 * (4.0 * pi * k as f64 / (n - 1) as f64).cos();
                s * w
            })
            .collect();
        let phase = (0..l)
            .map(|p| {
                let mut c = [0f32; UP_TAPS];
                let mut sum = 0.0f64;
                for j in 0..UP_TAPS {
                    sum += h[p + j * l];
                }
                for j in 0..UP_TAPS {
                    c[j] = (h[p + j * l] / sum) as f32; // каждая фаза с единичным усилением
                }
                c
            })
            .collect();
        Upsampler { l, phase, buf: vec![0.0; UP_TAPS - 1] }
    }

    /// Поднять кусок в уже выделенный буфер `out`.
    pub fn run_into(&mut self, input: &[i16], out: &mut Vec<i16>) {
        let hist = UP_TAPS - 1;
        self.buf.truncate(hist);
        self.buf.extend(input.iter().map(|&v| v as f32));
        out.clear();
        out.reserve(input.len() * self.l);
        for i in 0..input.len() {
            let c = hist + i;
            for p in 0..self.l {
                let mut acc = 0.0f32;
                for (j, k) in self.phase[p].iter().enumerate() {
                    acc += k * self.buf[c - j];
                }
                out.push(acc.round().clamp(-32767.0, 32767.0) as i16);
            }
        }
        self.buf.copy_within(input.len().., 0);
        self.buf.truncate(hist);
    }

    /// Поднять кусок; состояние фильтра переходит между кусками, стыков не слышно.
    pub fn run(&mut self, input: &[i16]) -> Vec<i16> {
        let mut out = Vec::new();
        self.run_into(input, &mut out);
        out
    }
}

/// Мягкий потолок в `f32`.
#[inline]
pub fn soft_clip_f32(x: f32) -> i16 {
    const KNEE: f32 = 29000.0;
    const TOP: f32 = 32767.0;
    let y = if x > KNEE {
        KNEE + (TOP - KNEE) * ((x - KNEE) / (TOP - KNEE)).tanh()
    } else if x < -KNEE {
        -KNEE - (TOP - KNEE) * ((-x - KNEE) / (TOP - KNEE)).tanh()
    } else {
        x
    };
    y as i16
}

/// Мягкий потолок: редкие остаточные пики плавно прижимаются к 32767, а не обрезаются с треском.
pub fn soft_clip(x: f64) -> i16 {
    const KNEE: f64 = 29000.0;
    const TOP: f64 = 32767.0;
    let y = if x > KNEE {
        KNEE + (TOP - KNEE) * ((x - KNEE) / (TOP - KNEE)).tanh()
    } else if x < -KNEE {
        -KNEE - (TOP - KNEE) * ((-x - KNEE) / (TOP - KNEE)).tanh()
    } else {
        x
    };
    y as i16
}

/// Усилить на месте (дБ); громкие места мягко прижимаются.
pub fn gain_pcm(pcm: &mut [i16], db: i64) {
    if db <= 0 {
        return;
    }
    let k = 10f32.powf(db as f32 / 20.0);
    for v in pcm.iter_mut() {
        let mut x = *v as f32 * k;
        if !(-32767.0..=32767.0).contains(&x) {
            x = x.clamp(-40000.0, 40000.0);
        }
        *v = soft_clip_f32(x);
    }
}

/// Ограничитель с опережением на 5 мс и удержанием пика по всему окну задержки:
/// громкие слоги заранее и плавно прижимаются к потолку без клиппинга и без срыва на переходах через ноль.
pub struct Limiter {
    buf: Vec<f32>,
    pos: usize,
    seq: u64,
    peaks: VecDeque<(u64, f32)>,
    gain: f32,
    attack: f32,
    release: f32,
}

impl Limiter {
    pub fn new(rate: usize) -> Self {
        let rate = rate.max(8_000);
        let len = (rate / 200).max(16);
        let attack = 1.0 - (-1.0 / (0.001 * rate as f32)).exp();
        let release = 1.0 - (-1.0 / (0.120 * rate as f32)).exp();
        Limiter {
            buf: vec![0.0; len],
            pos: 0,
            seq: 0,
            peaks: VecDeque::with_capacity(len + 1),
            gain: 1.0,
            attack,
            release,
        }
    }

    #[inline]
    pub fn next_f32(&mut self, x: f32) -> i16 {
        const CEIL: f32 = 29000.0;
        let a = x.abs();
        let idx = self.seq;
        self.seq = self.seq.wrapping_add(1);
        let window = self.buf.len() as u64;
        while let Some(&(old, _)) = self.peaks.front() {
            if idx.wrapping_sub(old) >= window {
                self.peaks.pop_front();
            } else {
                break;
            }
        }
        while let Some(&(_, val)) = self.peaks.back() {
            if val <= a {
                self.peaks.pop_back();
            } else {
                break;
            }
        }
        self.peaks.push_back((idx, a));

        let delayed = self.buf[self.pos];
        self.buf[self.pos] = x;
        self.pos += 1;
        if self.pos == self.buf.len() {
            self.pos = 0;
        }

        let window_peak = self.peaks.front().map_or(a, |&(_, v)| v).max(delayed.abs());
        let target = if window_peak > CEIL { CEIL / window_peak } else { 1.0 };
        if target < self.gain {
            self.gain += (target - self.gain) * self.attack;
        } else {
            self.gain += (target - self.gain) * self.release;
        }
        let delayed_abs = delayed.abs();
        if delayed_abs * self.gain > CEIL {
            self.gain = CEIL / delayed_abs;
        }
        soft_clip_f32(delayed * self.gain)
    }

    pub fn next(&mut self, x: f64) -> i16 {
        self.next_f32(x as f32)
    }
}

/// WAV моно s16le.
pub fn wav(pcm: &[i16], rate: u32) -> Vec<u8> {
    let mut b = Vec::with_capacity(44 + 2 * pcm.len());
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + 2 * pcm.len() as u32).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&rate.to_le_bytes());
    b.extend_from_slice(&(rate * 2).to_le_bytes());
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&(2 * pcm.len() as u32).to_le_bytes());
    for v in pcm {
        b.extend_from_slice(&v.to_le_bytes());
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsampler_keeps_dc_and_length() {
        let mut u = Upsampler::new(3);
        let out = u.run(&vec![1000i16; 200]);
        assert_eq!(out.len(), 600);
        assert!(out[300..].iter().all(|&v| (v - 1000).abs() <= 1), "{:?}", &out[300..310]);
    }

    #[test]
    fn limiter_caps_peaks() {
        let mut l = Limiter::new(16000);
        let out: Vec<i16> = (0..4000).map(|i| l.next(if i % 2 == 0 { 60000.0 } else { -60000.0 })).collect();
        assert!(out[200..].iter().all(|v| v.unsigned_abs() <= 32767));
        assert_eq!(soft_clip(1000.0), 1000);
    }

    #[test]
    fn limiter_holds_gain_across_zero_crossings_without_hard_clipping() {
        let mut l = Limiter::new(48000);
        // Сигнал 440 Гц с пиком 150 000 (имитация сильного усиления тихого голоса)
        let out: Vec<i16> = (0..4800)
            .map(|i| {
                let s = (i as f64 * std::f64::consts::TAU * 440.0 / 48000.0).sin() * 150_000.0;
                l.next_f32(s as f32)
            })
            .collect();
        // После 5 мс задержки (240 сэмплов) ни один пик не должен упираться в клиппинг > 29100
        let steady = &out[480..];
        let max_abs = steady.iter().map(|v| v.unsigned_abs()).max().unwrap();
        assert!(max_abs <= 29100, "unexpected clipping peak: {max_abs}");
        assert!(max_abs >= 27000, "limiter over-attenuated signal: {max_abs}");
    }
}
