//! Band-limited rate conversion, including the permitted drift range.
//!
//! rubato's polynomial interpolation has no anti-alias filter. Use sinc
//! filtering even for equal nominal rates: drift adjustments can also lower
//! the effective output Nyquist frequency.

use rubato::{
    Async, FixedAsync, ResamplerConstructionError, SincInterpolationParameters,
    SincInterpolationType, WindowFunction, calculate_cutoff,
};

use crate::ring::CHANNELS;

pub const CORRECTION_LIMIT: f64 = 0.02;
const SINC_LEN: usize = 256;
// Leave a tiny numerical margin for ratio multiplication/division at the
// control limits; it does not enlarge the controller's permitted range.
const MAX_RATIO_RELATIVE: f64 = 1.0 / (1.0 - CORRECTION_LIMIT) + 1e-9;

pub fn new(
    source_rate: u32,
    device_rate: u32,
    chunk: usize,
) -> Result<Async<f32>, ResamplerConstructionError> {
    let ratio = f64::from(device_rate) / f64::from(source_rate);
    let window = WindowFunction::BlackmanHarris2;
    // rubato scales the cutoff for the ORIGINAL nominal ratio only. Its
    // filters are not rebuilt when drift changes the ratio. Reserve enough
    // bandwidth for the lowest allowed output/input ratio from the outset.
    let cutoff_scale = (ratio * (1.0 - CORRECTION_LIMIT)).min(1.0) / ratio.min(1.0);
    let parameters = SincInterpolationParameters {
        sinc_len: SINC_LEN,
        f_cutoff: calculate_cutoff::<f32>(SINC_LEN, window) * cutoff_scale as f32,
        oversampling_factor: 128,
        interpolation: SincInterpolationType::Cubic,
        window,
    };
    Async::new_sinc(
        ratio,
        MAX_RATIO_RELATIVE,
        &parameters,
        chunk,
        CHANNELS,
        FixedAsync::Output,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rubato::Resampler;
    use rubato::audioadapter_buffers::direct::InterleavedSlice;

    /// Measure the steady-state output of a source sine. No Windows API or
    /// reference copy of the filter is used: this checks the actual DSP.
    fn sine_rms(source_rate: u32, device_rate: u32, frequency: f64, correction: f64) -> f64 {
        let mut resampler = new(source_rate, device_rate, 480).unwrap();
        resampler
            .set_resample_ratio_relative(1.0 - correction, false)
            .unwrap();
        let mut input_position = 0;
        let mut energy = 0.0;
        let mut count = 0;
        for block in 0..24 {
            let frames = resampler.input_frames_next();
            let input: Vec<f32> = (0..frames)
                .flat_map(|i| {
                    let value = (std::f64::consts::TAU * frequency * (input_position + i) as f64
                        / f64::from(source_rate))
                    .sin() as f32;
                    [value, value]
                })
                .collect();
            input_position += frames;
            let input = InterleavedSlice::new(&input, CHANNELS, frames).unwrap();
            let mut output = vec![0.0; 480 * CHANNELS];
            let mut adapter = InterleavedSlice::new_mut(&mut output, CHANNELS, 480).unwrap();
            resampler
                .process_into_buffer(&input, &mut adapter, None)
                .unwrap();
            if block >= 4 {
                energy += output.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>();
                count += output.len();
            }
        }
        (energy / count as f64).sqrt()
    }

    #[test]
    fn passband_is_preserved_across_nominal_rates_and_drift_limits() {
        for (source, device) in [
            (48000, 44100),
            (96000, 48000),
            (44100, 48000),
            (48000, 48000),
        ] {
            for correction in [-CORRECTION_LIMIT, 0.0, CORRECTION_LIMIT] {
                let rms = sine_rms(source, device, 1000.0, correction);
                assert!(
                    (rms - std::f64::consts::FRAC_1_SQRT_2).abs() < 0.01,
                    "{source}->{device}: {rms}"
                );
            }
        }
    }

    #[test]
    fn out_of_band_tones_are_suppressed_instead_of_aliased() {
        for (source, device, tone) in [
            (48000, 44100, 23000.0),
            (96000, 48000, 30000.0),
            (192000, 48000, 60000.0),
        ] {
            for correction in [-CORRECTION_LIMIT, 0.0, CORRECTION_LIMIT] {
                let rms = sine_rms(source, device, tone, correction);
                assert!(
                    rms < 0.001,
                    "{source}->{device}, correction={correction}: alias RMS {rms}"
                );
            }
        }
    }
}
