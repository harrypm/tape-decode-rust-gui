use anyhow::{bail, Context as _, Result};
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};
use rustfft::num_complex::{Complex32, Complex64};
use rustfft::{Fft, FftPlanner};
use sci_rs::signal::filter::design::{FilterBandType, Sos};
use std::sync::Arc;

use crate::decode::{
    gen_chroma_heterodyne, ChromaFftFilter, ChromaSepClass, BLOCKCUT, BLOCKCUT_END, BLOCKSIZE,
    SECAM_LO_TRIM_LIMIT_HZ,
};
use crate::optimized::{narrow_sos, SincLut};
use crate::request::{
    BoostRampFilter, ColorSystem, DecodeOptions, DecodeRequest, DecoderParams, DeemphasisParams,
    FieldOrderAction, Ire0Adjust, LineSystem, NonlinearParams, ShelfKind, SysParams,
    VideoLumaFilter, WowInterpolation,
};

const CHROMA_AUDIO_NOTCH_Q: f64 = 10.0;

/// Input sample rates the decoder accepts, in MHz. Below the low end the
/// fixed 0.5 MHz sync low-pass and 700 kHz envelope smoother have no room
/// under the Nyquist frequency; no capture device samples anywhere near the
/// high end, which keeps a rate given in hertz without its suffix from
/// sizing the buffers by it.
const INPUT_RATE_MHZ: std::ops::RangeInclusive<f64> = 2.0..=1000.0;

fn store_real_filter(values: Vec<f64>) -> Vec<f32> {
    values.into_iter().map(|value| value as f32).collect()
}

fn store_complex_filter(values: Vec<Complex64>) -> Vec<Complex32> {
    values
        .into_iter()
        .map(|value| Complex32::new(value.re as f32, value.im as f32))
        .collect()
}

// Narrow a designed SOS cascade to f32 for the native-f32 filter runtime.
fn store_sos_filter(sos: Vec<Sos<f64>>) -> Vec<Sos<f32>> {
    narrow_sos(&sos)
}

pub struct DecoderSpec {
    pub(crate) freq: f64,

    pub(crate) color_system: ColorSystem,

    pub(crate) sys_fsc_mhz: f64,
    pub(crate) sys_frame_lines: LineSystem,
    pub(crate) sys_field_lines: [i64; 2],
    pub(crate) sys_line_period: f64,
    pub(crate) sys_active_video_us: [f64; 2],
    pub(crate) sys_fps: f64,
    pub(crate) sys_ire0: f32,
    pub(crate) sys_hz_ire: f32,
    pub(crate) sys_vsync_ire: f32,
    pub(crate) sys_color_burst_us: [f64; 2],
    /// Color burst window in output samples from the line start, padded a
    /// little past the nominal burst, used by both the burst lock and the
    /// chroma upconvert. Its length is a whole number of subcarrier cycles
    /// (four samples each) so the correlation against the reference
    /// subcarrier is unbiased.
    pub(crate) burst_area: (usize, usize),
    pub(crate) sys_blacksnr_slice: [usize; 3],
    pub(crate) sys_num_pulses: usize,
    pub(crate) sys_hsync_pulse_us: f64,
    pub(crate) sys_eq_pulse_us: f64,
    pub(crate) sys_vsync_pulse_us: f64,
    pub(crate) sys_output_zero: i64,
    pub(crate) sys_outlinelen: usize,
    pub(crate) sys_ld_vits_whitelocs: Vec<[usize; 3]>,
    pub(crate) sys_burst_abs_ref: Option<f32>,
    pub(crate) sys_track_ire0_offset: [f32; 2],

    pub(crate) decoder_color_under_carrier: f64,
    pub(crate) decoder_chroma_conversion_lo: Option<f64>,
    pub(crate) decoder_chroma_carrier_mult: Option<u32>,
    pub(crate) decoder_chroma_rotation: Option<[i64; 2]>,
    pub(crate) decoder_chroma_offset: f64,
    pub(crate) decoder_nonlinear_highpass_limit_l: f32,
    pub(crate) decoder_nonlinear_highpass_limit_h: f32,
    pub(crate) decoder_nonlinear_exp_scaling: f32,
    pub(crate) decoder_nonlinear_scaling_1: Option<f32>,
    pub(crate) decoder_nonlinear_scaling_2: Option<f32>,
    pub(crate) decoder_nonlinear_logistic: Option<(f32, f32)>,
    pub(crate) decoder_nonlinear_static_factor: Option<f32>,

    pub(crate) field_order_action: FieldOrderAction,
    // Sharpness EQ: the zero-phase highpass added back onto the demod has the
    // purely real transfer |H|^2, so the whole effect is one spectrum gain
    // (1 + sharpness*gain*|H|^2) over the unique block bins, applied with the
    // r2c/c2r block transforms instead of a high-order time-domain cascade.
    pub(crate) video_eq_fft_gain: Option<Vec<f32>>,

    pub(crate) chroma_afc_narrowband: Vec<Vec<Sos<f32>>>,
    pub(crate) chroma_afc_fine_tune_fh_ratio: f64,
    // Whole-field analytic-signal transform pair (the ME-SECAM carrier servo
    // and the SECAM method 1 restoration): a real forward transform and the
    // complex inverse, at a fast length padded past the field. None unless
    // one of them runs.
    pub(crate) secam_analytic_fft: Option<(Arc<dyn RealToComplex<f32>>, Arc<dyn Fft<f32>>)>,

    pub(crate) dod_threshold_p: f32,
    pub(crate) dod_threshold_a: Option<f32>,
    pub(crate) dod_hysteresis: f32,

    pub(crate) rf_chroma_heterodyne: Vec<Vec<f32>>,
    pub(crate) rf_fsc_wave: Vec<(f32, f32)>,

    pub(crate) rf_disable_comb: bool,
    pub(crate) rf_disable_right_hsync: bool,
    pub(crate) rf_fallback_vsync: bool,
    pub(crate) rf_field_order_confidence: i64,
    pub(crate) rf_saved_levels: bool,
    pub(crate) rf_y_comb: f32,
    pub(crate) rf_write_chroma: bool,
    pub(crate) chroma_output: bool,
    pub(crate) rf_skip_hsync_refine: bool,
    pub(crate) rf_export_raw_tbc: bool,
    pub(crate) rf_ire0_adjust: Ire0Adjust,
    pub(crate) rf_relaxed_line0: bool,
    pub(crate) rf_detect_chroma_track_phase: bool,
    pub(crate) rf_enable_color_killer: bool,
    pub(crate) rf_disable_burst_hsync: bool,
    pub(crate) rf_disable_phase_correction: bool,
    pub(crate) rf_secam_carrier_servo: bool,
    pub(crate) secam_lo_trim: Option<f64>,
    pub(crate) cti_mix: f32,
    pub(crate) cti_width: u32,

    /// Zero-phase gain of the block-level burst chain over the unique block
    /// bins; `None` without chroma processing.
    pub(crate) chroma_burst_block_fft_gain: Option<Vec<f32>>,
    pub(crate) chroma_filter_deemphasis: Option<Vec<Sos<f32>>>,
    pub(crate) chroma_filter_final: Vec<Sos<f32>>,
    // fsc-anchored field-wide band-pass of the upconverted chroma; the SECAM
    // variants restore their block elsewhere and keep the block-anchored
    // `chroma_filter_final`.
    pub(crate) chroma_fft_filter: Option<ChromaFftFilter>,
    // Post-TBC band-pass around the SECAM method 1 under carriers, ahead of
    // the phase multiplication.
    pub(crate) secam_under_filter: Option<Vec<Sos<f32>>>,

    pub(crate) video_rf_filter: Vec<f32>,
    pub(crate) video_notch_filter: Option<Vec<f32>>,
    pub(crate) video_env_post_filter: Vec<Sos<f32>>,
    pub(crate) video_rf_top_fft_gain: Option<Vec<f32>>,
    pub(crate) video_high_boost_value: Option<f32>,
    pub(crate) video_disable_diff_demod: bool,
    pub(crate) video_chroma_trap: Option<ChromaSepClass>,
    pub(crate) video_filter: Vec<Complex32>,
    pub(crate) video_nl_amplitude_lpf: Vec<Sos<f32>>,
    pub(crate) video_nl_high_pass_f: Option<Vec<Complex32>>,
    pub(crate) video_nldeemp_enabled: bool,
    pub(crate) video_subdeemp_enabled: bool,
    /// Deviation the sub-deemphasis measures its amplitude against, in Hz.
    pub(crate) video_sub_deemphasis_deviation: f32,
    pub(crate) video_fsc_notch: Option<Vec<Sos<f32>>>,
    pub(crate) video05_filter: Vec<Complex32>,

    pub(crate) fft_block_inverse_f32: Arc<dyn Fft<f32>>,
    pub(crate) fft_block_r2c_f32: Arc<dyn RealToComplex<f32>>,
    pub(crate) fft_block_c2r_f32: Arc<dyn ComplexToReal<f32>>,
    pub(crate) fft_field_forward_f32: Arc<dyn Fft<f32>>,

    /// Black pedestal above blanking in IRE.
    pub(crate) black_ire: f64,
    pub(crate) track_phase: Option<i64>,
    pub(crate) wow_level_adjust_smoothing: f32,
    pub(crate) wow_interpolation_method: WowInterpolation,
    // TBC resampling weights, built once and shared by every field.
    pub(crate) sinc_lut: SincLut,
    pub(crate) do_dod: bool,
    pub(crate) level_adjust: f32,
    _private: (),
}

impl DecoderSpec {
    pub fn new(request: &DecodeRequest) -> Result<Self> {
        let sys_params = &request.decode_profile.sys_params;
        let decoder_params = &request.decode_profile.decoder_params;
        let decode_options = &request.decode_profile.decode_options;
        let rf_freq = request.inputfreq;
        if !INPUT_RATE_MHZ.contains(&rf_freq) {
            bail!(
                "--frequency must be an input sample rate from {} to {} MHz, not {rf_freq} (a rate in hertz or kilohertz needs its hz or khz suffix)",
                INPUT_RATE_MHZ.start(),
                INPUT_RATE_MHZ.end()
            );
        }
        let freq_half = rf_freq / 2.0;
        let freq_hz = rf_freq * 1_000_000.0;
        let freq_hz_half = freq_hz / 2.0;
        let rf_color_system = if decoder_params.is_composite_color {
            ColorSystem::Monochrome
        } else {
            sys_params.color_system
        };
        check_profile(sys_params, decoder_params, decode_options)?;
        // System M, 525-line NTSC-M and PAL-M alike, sets black up 7.5 IRE
        // above blanking; NTSC-J and the other standards keep it at 0 IRE.
        let black_ire = if sys_params.frame_lines == LineSystem::Line525 && !request.ntscj {
            7.5
        } else {
            0.0
        };
        // cafc measures a single carrier peak, which the line-alternating
        // two-carrier SECAM FM chroma does not have.
        let do_cafc = if request.cafc && rf_color_system == ColorSystem::Secam {
            tracing::warn!(
                "cafc is not supported for SECAM systems, ignoring (for ME-SECAM, the carrier servo tunes the conversion LO instead)"
            );
            false
        } else {
            request.cafc
        };
        // Controls the sharpness EQ gain.
        let sharpness_level = request.sharpness as f64 / 100.0;
        let video_eq_fft_gain = if sharpness_level > f64::EPSILON {
            let loband = &decoder_params
                .video_eq
                .as_ref()
                .context("sharpness requires video_eq params")?
                .loband;
            let sos = iir_highpass_sos(
                "video_eq.loband",
                freq_hz,
                loband.corner,
                loband.transition,
                loband.order_limit,
            )?;
            let gain = loband.order_limit as f64;
            let response = sosfiltfft(&sos, BLOCKSIZE);
            Some(
                response[..BLOCKSIZE / 2 + 1]
                    .iter()
                    .map(|h| (sharpness_level * gain).mul_add(h.norm_sqr(), 1.0) as f32)
                    .collect(),
            )
        } else {
            None
        };
        let fh = sys_params.fps * sys_params.frame_lines.line_count() as f64;
        let color_under = decoder_params.color_under_carrier;
        let conversion_lo = decoder_params.chroma_conversion_lo;
        let carrier_mult = decoder_params.chroma_carrier_mult;
        if conversion_lo.is_some() && carrier_mult.is_some() {
            bail!("chroma_conversion_lo and chroma_carrier_mult are mutually exclusive");
        }
        // Both SECAM variants restore the chroma block to the same place:
        // ME-SECAM by mixing against the conversion LO, method 1 by
        // multiplying the carrier up.
        let secam_block_center_mhz = match (conversion_lo, carrier_mult) {
            (Some(conversion_lo), _) => Some((conversion_lo - color_under) / 1e6),
            (None, Some(mult)) => Some(color_under * f64::from(mult) / 1e6),
            (None, None) => None,
        };
        let out_sample_rate_mhz = sys_params.fsc_mhz * 4.0;
        let out_sample_rate_hz = out_sample_rate_mhz * 1e6;
        let fsc_hz = sys_params.fsc_mhz * 1e6;
        // The rate the TBC output is actually clocked at (outlinelen samples
        // per line period), which 4 * fsc only approximates after outlinelen
        // is rounded to whole samples.
        let tbc_sample_rate = sys_params.outlinelen as f64 * 1e6 / sys_params.line_period;
        let fieldlen =
            sys_params.outlinelen * *sys_params.field_lines.iter().max().unwrap() as usize;
        let burst_area = burst_area(sys_params.color_burst_us, tbc_sample_rate);
        // The burst lock reads each line as far past the burst window as the
        // window sits from the line start.
        if burst_area.0 + burst_area.1 > sys_params.outlinelen {
            bail!(
                "color_burst_us: the burst lock's read window of {} output samples runs past the {}-sample line",
                burst_area.0 + burst_area.1,
                sys_params.outlinelen
            );
        }

        // Standard frequency color carrier wave.
        let fsc_wave = gen_wave_at_frequency(sys_params.fsc_mhz, out_sample_rate_mhz, fieldlen);

        let chroma_afc_narrowband = if do_cafc {
            const TRANSITION_EXPAND: f64 = 12.0;
            // Carrier-frequency tolerance (percent), expanded into a transition
            // width. The high and low transitions are symmetric, so one value.
            let percent = 200.0 * fh / color_under;
            let trans = color_under * TRANSITION_EXPAND * percent / 100.0;
            let what = "color_under_carrier";
            vec![
                store_sos_filter(iir_highpass_sos(
                    what,
                    out_sample_rate_hz,
                    color_under,
                    trans,
                    200,
                )?),
                store_sos_filter(iir_lowpass_sos(
                    what,
                    out_sample_rate_hz,
                    color_under,
                    trans,
                    200,
                )?),
            ]
        } else {
            Vec::new()
        };

        let blocklen = BLOCKSIZE;
        let mut fft_planner_f32 = FftPlanner::<f32>::new();
        let fft_block_inverse_f32 = fft_planner_f32.plan_fft_inverse(blocklen);
        let fft_field_forward_f32 = fft_planner_f32.plan_fft_forward(fieldlen);
        // Half-spectrum transforms for the real-valued block signals: the
        // forward r2c produces only the blocklen/2 + 1 unique bins and the c2r
        // inverse consumes the same, so each runs on a half-length inner FFT.
        let mut fft_real_planner = RealFftPlanner::<f32>::new();
        let fft_block_r2c_f32 = fft_real_planner.plan_fft_forward(blocklen);
        let fft_block_c2r_f32 = fft_real_planner.plan_fft_inverse(blocklen);

        let is_color_under = rf_color_system != ColorSystem::Monochrome;

        let video_subdeemp_enabled =
            decoder_params.nonlinear.use_sub_deemphasis || request.subdeemp;
        let video_sub_deemphasis_deviation = sys_params
            .nonlinear_deviation
            .unwrap_or(sys_params.hz_ire * (100.0 - sys_params.vsync_ire));
        if video_subdeemp_enabled && video_sub_deemphasis_deviation == 0.0 {
            bail!("sub-deemphasis requires a non-zero deviation");
        }
        let chroma_deemphasis_enabled = decoder_params.chroma_deemphasis_enabled;
        let fm_audio_notch = request.fm_audio_notch;
        // Chroma processing, which also locks hsync to the burst, runs for every
        // color-under format; only producing the chroma output is optional.
        let rf_write_chroma = is_color_under;
        let chroma_output = rf_write_chroma && !request.skip_chroma;
        if chroma_output && sys_params.burst_abs_ref.is_none() {
            bail!("chroma output requires the format's burst_abs_ref");
        }

        let rf_disable_comb = request.disable_comb || rf_color_system == ColorSystem::Secam;
        // SECAM has no phase-locked burst: its "burst" is an FM carrier whose
        // phase carries no timing, so locking hsync to it only injects
        // sub-pixel jitter into both planes.
        let rf_disable_burst_hsync =
            request.rf_disable_burst_hsync || rf_color_system == ColorSystem::Secam;
        // The chroma transient improvement reads a sample and its predecessor
        // as a quadrature pair of the subcarrier, which SECAM's FM chroma is
        // not: pulling its samples about distorts the carrier frequency.
        let cti_mix = if rf_color_system == ColorSystem::Secam {
            0.0
        } else {
            request.cti_mix
        };
        let rf_fallback_vsync = request.fallback_vsync;
        if request.y_comb.is_nan() || request.y_comb < 0.0 {
            bail!("--y-comb must be a non-negative IRE clamp");
        }
        // The transient improvement's noise gate scales with the square root
        // of the width, so a width of 0 passes every transition.
        if request.cti_width == 0 {
            bail!("--cti-width must be at least 1");
        }
        if !(0.0..=1.0).contains(&request.cti_mix) {
            bail!("--cti-mix must be a mix from 0 to 1");
        }
        if request.sharpness > 100 {
            bail!("--sharpness must be from 0 to 100");
        }
        if !request.level_adjust.is_finite() {
            bail!("--level-adjust must be a finite fraction");
        }
        if let Some(trim) = request.secam_lo_trim {
            if !(-SECAM_LO_TRIM_LIMIT_HZ..=SECAM_LO_TRIM_LIMIT_HZ).contains(&trim) {
                bail!(
                    "--secam-lo-trim must be within {SECAM_LO_TRIM_LIMIT_HZ} Hz either way, the servo's range"
                );
            }
        }
        if fm_audio_notch.is_nan() || fm_audio_notch < 0.0 {
            bail!("--fm-audio-notch must be 0 (no notch) or a positive Q");
        }
        if let Some(high_boost) = request.high_boost {
            if !high_boost.is_finite() || high_boost < 0.0 {
                bail!("--high-boost must be a non-negative multiplier");
            }
        }
        if !(request.dod_threshold_p.is_finite() && request.dod_threshold_p > 0.0) {
            bail!("--dod-threshold-p must be a positive fraction");
        }
        if let Some(threshold) = request.dod_threshold_a {
            if !(threshold.is_finite() && threshold >= 0.0) {
                bail!("--dod-threshold-a must be a non-negative level");
            }
        }
        if !(request.dod_hysteresis.is_finite() && request.dod_hysteresis >= 1.0) {
            bail!("--dod-hysteresis must be a ratio of at least 1");
        }
        let rf_y_comb = request.y_comb * sys_params.hz_ire;

        // Filter for rf before demodulating.
        // Only use bpf if video_bpf is defined - otherwise skip.
        let y_fm = if let Some(dp_video_bpf) = &decoder_params.video_bpf {
            let (b, a) = butter_ba(
                "video_bpf",
                dp_video_bpf.order,
                &[dp_video_bpf.low, dp_video_bpf.high],
                freq_hz,
                FilterBandType::Bandpass,
            )?;
            Some(filtfft(&b, &a, blocklen, true))
        } else {
            None
        };
        let y_fm_lowpass = sosfiltfft(
            &butter_sos(
                "video_lpf_extra",
                decoder_params.video_lpf_extra_order,
                &[decoder_params.video_lpf_extra],
                freq_hz,
                FilterBandType::Lowpass,
            )?,
            blocklen,
        );
        let y_fm_highpass = sosfiltfft(
            &butter_sos(
                "video_hpf_extra",
                decoder_params.video_hpf_extra_order,
                &[decoder_params.video_hpf_extra],
                freq_hz,
                FilterBandType::Highpass,
            )?,
            blocklen,
        );
        let y_fm_lowpass_abs = abs_complex(y_fm_lowpass);
        let y_fm_highpass_abs = abs_complex(y_fm_highpass);
        let mut video_rf_filter = if let Some(y_fm) = y_fm {
            let y_fm_abs = abs_complex(y_fm);
            multiply(&multiply(&y_fm_abs, &y_fm_lowpass_abs), &y_fm_highpass_abs)
        } else {
            multiply(&y_fm_lowpass_abs, &y_fm_highpass_abs)
        };

        // Add optional rf peaking filter.
        if let Some(dp_video_rf_peak) = &decoder_params.video_rf_peak {
            let peaking_filter = peaking(
                "video_rf_peak",
                dp_video_rf_peak.freq,
                dp_video_rf_peak.bandwidth,
                dp_video_rf_peak.gain,
                freq_hz,
            )?;
            let peaking_fft = filtfft(&peaking_filter.0, &peaking_filter.1, blocklen, true);
            video_rf_filter = multiply(&video_rf_filter, &abs_complex(peaking_fft));
        }

        if fm_audio_notch > f64::EPSILON {
            if let Some(fm_audio_channels) = &decoder_params.fm_audio_channels {
                let notch0 = gen_fft_notch(
                    "fm_audio_channels.channel_0_freq",
                    fm_audio_channels.channel_0_freq,
                    fm_audio_notch,
                    freq_hz,
                    blocklen,
                )?;
                let notch1 = gen_fft_notch(
                    "fm_audio_channels.channel_1_freq",
                    fm_audio_channels.channel_1_freq,
                    fm_audio_notch,
                    freq_hz,
                    blocklen,
                )?;
                let audio_fm_notch_filter = multiply(&notch0, &notch1);
                video_rf_filter =
                    multiply(&video_rf_filter, &abs_complex(audio_fm_notch_filter));
            } else {
                bail!("Requested audio notch even though the format does not have audio channels specified!");
            }
        }

        if let Some(boost_ramp) = &decoder_params.boost_ramp {
            let ramp = gen_ramp_filter(boost_ramp, freq_hz_half, blocklen);
            video_rf_filter = multiply(&video_rf_filter, &ramp);
        }

        // The high-boost band extraction is zero-phase, so it reduces to the
        // real |H|^2 spectrum gain over the unique block bins, applied straight
        // to the already-available RF spectrum instead of round-tripping
        // through a time-domain forward/backward cascade.
        let video_rf_top_fft_gain = if let Some(boost_bpf) = &decoder_params.boost_bpf {
            let sos = butter_sos(
                "boost_bpf",
                1,
                &[boost_bpf.low, boost_bpf.high],
                freq_hz,
                FilterBandType::Bandpass,
            )?;
            let response = sosfiltfft(&sos, BLOCKSIZE);
            Some(
                response[..BLOCKSIZE / 2 + 1]
                    .iter()
                    .map(|h| h.norm_sqr() as f32)
                    .collect::<Vec<f32>>(),
            )
        } else {
            None
        };

        let filter_deemp = if let Some(deemph) = &decoder_params.deemph {
            gen_video_main_deemp_fft(deemph, freq_hz, blocklen)?
        } else {
            vec![Complex64::new(1.0, 0.0); blocklen / 2 + 1]
        };

        let filter_video_lpf_real = if decoder_params.video_lpf_supergauss {
            gen_video_lpf_supergauss(
                decoder_params.video_lpf_freq,
                decoder_params.video_lpf_order,
                freq_hz_half,
                blocklen,
            )
        } else {
            gen_video_lpf(
                decoder_params.video_lpf_freq,
                decoder_params.video_lpf_order,
                freq_hz,
                blocklen,
            )?
        };

        let custom_video =
            if let Some(custom_luma_filters) = &decoder_params.video_custom_luma_filters {
                gen_custom_video_filters(custom_luma_filters, freq_hz, blocklen)?
            } else {
                vec![Complex64::new(1.0, 0.0); blocklen / 2 + 1]
            };

        // Sync detection uses a fixed 0.5 MHz low-pass FIR so the delay and
        // coefficient response stay stable.
        let f0_5 = firwin_lowpass(65, 0.5 / freq_half);
        let filter_05 = filtfft(&f0_5, &[1.0], blocklen, false);

        let deemp_lpf = multiply(&filter_deemp, &filter_video_lpf_real);
        let video_filter = multiply(&deemp_lpf, &custom_video);
        let video05_filter = multiply(&deemp_lpf, &filter_05);

        // The post-envelope smoother is intentionally first-order; higher
        // orders alter the detector response too much.
        let video_env_post_filter = butter_sos(
            "the envelope smoother",
            1,
            &[700e3],
            freq_hz,
            FilterBandType::Lowpass,
        )?;

        let video_nl_amplitude_lpf = butter_sos(
            "nonlinear.amp_lpf_freq",
            1,
            &[decoder_params.nonlinear.amp_lpf_freq],
            freq_hz,
            FilterBandType::Lowpass,
        )?;
        let video_fsc_notch = if decode_options.use_fsc_notch_filter {
            Some(biquad_sos(iirnotch("the fsc notch", fsc_hz, 2.0, freq_hz)?))
        } else {
            None
        };
        let video_nl_high_pass_f = if request.video_nldeemp_enabled || video_subdeemp_enabled {
            if decoder_params.nonlinear.highpass_freq <= 0.0 {
                bail!(
                    "the format has no nonlinear deemphasis parameters (nonlinear.highpass_freq is not set), which --nldeemp and --subdeemp need"
                );
            }
            Some(gen_nonlinear_bandpass(
                &decoder_params.nonlinear,
                freq_hz,
                blocklen,
            )?)
        } else {
            None
        };
        let video_high_boost_value = decoder_params
            .boost_bpf
            .as_ref()
            .map(|boost_bpf| request.high_boost.unwrap_or(boost_bpf.mult) as f32);

        let chroma_bandpass_final = |color_under_format: bool| -> Result<Vec<Sos<f64>>> {
            let (band_low, band_high) = match secam_block_center_mhz {
                Some(center) if color_under_format => {
                    // Place the band around the restored SECAM chroma block
                    // rather than around fsc. The tight top edge suppresses
                    // high-side FM splatter from saturated transitions, which
                    // otherwise reaches downstream discriminators with wide
                    // take-off filters and turns into clicks and streaks at
                    // colour edges.
                    (center - 0.67, center + 0.55)
                }
                _ if color_under_format => (
                    sys_params.fsc_mhz - (color_under / 1e6) * 0.9,
                    sys_params.fsc_mhz + (color_under / 1e6) * 0.75,
                ),
                // Using a narrow filter atm as this is just used for picking out
                // burst signal in this case.
                _ => (sys_params.fsc_mhz - 0.1, sys_params.fsc_mhz + 0.1),
            };
            butter_sos(
                "the chroma band-pass",
                4,
                &[band_low * 1e6, band_high * 1e6],
                out_sample_rate_hz,
                FilterBandType::Bandpass,
            )
        };

        // --- Build luma notch filter for the RF/video path ---
        let (video_notch_filter, chroma_filter_video_notch) = if let Some(notch) = &request.notch {
            if notch.q.is_nan() || notch.q <= 0.0 {
                bail!("--notch-q must be positive");
            }
            let video_notch_raw = iirnotch("--notch", notch.freq * 1e6, notch.q, freq_hz)?;
            let video_notch_ba = ba_to_vec(video_notch_raw);
            let video_notch_filter = abs_complex(filtfft(
                &video_notch_ba.0,
                &video_notch_ba.1,
                blocklen,
                true,
            ));
            (Some(video_notch_filter), Some(biquad_sos(video_notch_raw)))
        } else {
            (None, None)
        };

        // --- Build chroma filters ---
        let chroma_filter_deemphasis = if chroma_deemphasis_enabled {
            Some(biquad_sos_vec(peaking(
                "the chroma deemphasis",
                fsc_hz,
                0.5e6,
                3.4,
                out_sample_rate_hz,
            )?))
        } else {
            None
        };

        let chroma_filter_audio_notch = if decoder_params.chroma_audio_notch_freq > f64::EPSILON {
            Some(biquad_sos(iirnotch(
                "chroma_audio_notch_freq",
                decoder_params.chroma_audio_notch_freq,
                CHROMA_AUDIO_NOTCH_Q,
                freq_hz,
            )?))
        } else {
            None
        };
        // Combined zero-phase response of the block-level chroma burst chain
        // (the burst bandpass plus the optional audio/video notches): each
        // filtfilt transfer is the purely real |H|^2, so the whole chain
        // collapses into one spectrum gain over the unique block bins.
        let chroma_burst_block_fft_gain = rf_write_chroma
            .then(|| -> Result<Vec<f32>> {
                let burst_bandpass = butter_sos(
                    "chroma_bpf",
                    decoder_params.chroma_bpf_order,
                    &[
                        decoder_params.chroma_bpf_lower,
                        decoder_params.chroma_bpf_upper,
                    ],
                    freq_hz,
                    FilterBandType::Bandpass,
                )?;
                let mut gain: Vec<f64> = sosfiltfft(&burst_bandpass, blocklen)[..blocklen / 2 + 1]
                    .iter()
                    .map(|h| h.norm_sqr())
                    .collect();
                for sos in [&chroma_filter_audio_notch, &chroma_filter_video_notch]
                    .into_iter()
                    .flatten()
                {
                    for (bin, h) in gain.iter_mut().zip(sosfiltfft(sos, blocklen)) {
                        *bin *= h.norm_sqr();
                    }
                }
                Ok(store_real_filter(gain))
            })
            .transpose()?;

        // Post-TBC chroma filter at output sample rate (4fsc).
        let chroma_filter_final = chroma_bandpass_final(is_color_under)?;
        let chroma_fft_filter = (is_color_under && secam_block_center_mhz.is_none()).then(|| {
            let chroma_len = sys_params.outlinelen * (sys_params.frame_lines.line_count() / 2 + 1);
            ChromaFftFilter::new(chroma_len, sys_params.fsc_mhz * 1e6, color_under)
        });
        let (rf_chroma_heterodyne, rf_fsc_wave) = if is_color_under {
            let het_wave_scale = match conversion_lo {
                // Mix against the recorder's conversion LO directly, at the
                // true TBC rate so the restored carriers land exactly on their
                // studio frequencies.
                Some(conversion_lo) => conversion_lo / tbc_sample_rate,
                None => (sys_params.fsc_mhz + color_under / 1e6) / out_sample_rate_mhz,
            };
            (
                gen_chroma_heterodyne(het_wave_scale, 0.0, fieldlen),
                fsc_wave,
            )
        } else {
            (Vec::new(), Vec::new())
        };
        let rf_secam_carrier_servo = request.rf_secam_carrier_servo;
        let secam_analytic_fft = if (conversion_lo.is_some() && rf_secam_carrier_servo)
            || carrier_mult.is_some()
        {
            // Zero-padded past the field so the Hilbert transform runs at a
            // fast length.
            let n = next_fast_len(fieldlen);
            Some((
                fft_real_planner.plan_fft_forward(n),
                fft_planner_f32.plan_fft_inverse(n),
            ))
        } else {
            None
        };
        // The band covers the under carrier pair (1.0625/1.1015625 MHz) plus
        // the +-126.5 kHz maximum deviation and as much sideband room as fits
        // below the luma FM area; out-of-band noise would turn straight into
        // phase noise, which the multiplication amplifies.
        let secam_under_filter = carrier_mult
            .map(|_| {
                butter_sos(
                    "the SECAM under carrier band-pass",
                    3,
                    &[550e3, 1300e3],
                    tbc_sample_rate,
                    FilterBandType::Bandpass,
                )
            })
            .transpose()?;

        let video_chroma_trap = if request.chroma_trap {
            Some(ChromaSepClass::new(freq_hz, sys_params.fsc_mhz))
        } else {
            None
        };

        let track_phase = request.track_phase;
        if let Some(track_phase) = track_phase {
            if rf_color_system == ColorSystem::Secam {
                bail!("Track phase is not supported for SECAM");
            }
            if track_phase != 0 && track_phase != 1 {
                bail!("Track phase can only be 0, 1 or None");
            }
            if decoder_params.chroma_rotation.is_none() {
                bail!("Track phase requires a format with chroma rotation");
            }
        }

        let wow_level_adjust_smoothing = request
            .wow_level_adjust_smoothing
            .unwrap_or(sys_params.frame_lines.line_count() as f32 / 2.0);
        // The level smoothing is a first-order IIR over the window in output
        // samples, stable only for a window of at least one sample; 0 disables
        // it.
        let smoothing_window_samples =
            f64::from(wow_level_adjust_smoothing) * sys_params.outlinelen as f64;
        if wow_level_adjust_smoothing.is_nan()
            || (wow_level_adjust_smoothing != 0.0 && smoothing_window_samples < 1.0)
        {
            bail!(
                "--wow-level-adjust-smoothing must be 0 (no smoothing) or at least one output sample, 1/{} lines",
                sys_params.outlinelen
            );
        }

        Ok(Self {
            field_order_action: request.field_order_action,
            video_eq_fft_gain,

            chroma_afc_narrowband,
            chroma_afc_fine_tune_fh_ratio: decode_options.chroma_afc_fine_tune_fh_ratio,
            secam_analytic_fft,

            freq: rf_freq,
            color_system: rf_color_system,
            sys_fsc_mhz: sys_params.fsc_mhz,
            sys_frame_lines: sys_params.frame_lines,
            sys_field_lines: sys_params.field_lines,
            sys_line_period: sys_params.line_period,
            sys_active_video_us: sys_params.active_video_us,
            sys_fps: sys_params.fps,
            sys_ire0: sys_params.ire0,
            sys_hz_ire: sys_params.hz_ire,
            sys_vsync_ire: sys_params.vsync_ire,
            sys_color_burst_us: sys_params.color_burst_us,
            burst_area,
            sys_blacksnr_slice: sys_params.blacksnr_slice,
            sys_num_pulses: sys_params.num_pulses,
            sys_hsync_pulse_us: sys_params.hsync_pulse_us,
            sys_eq_pulse_us: sys_params.eq_pulse_us,
            sys_vsync_pulse_us: sys_params.vsync_pulse_us,
            sys_output_zero: sys_params.output_zero,
            sys_outlinelen: sys_params.outlinelen,
            sys_ld_vits_whitelocs: sys_params.ld_vits_whitelocs.clone(),
            sys_burst_abs_ref: sys_params.burst_abs_ref,
            sys_track_ire0_offset: sys_params.track_ire0_offset,

            decoder_color_under_carrier: decoder_params.color_under_carrier,
            decoder_chroma_conversion_lo: conversion_lo,
            decoder_chroma_carrier_mult: carrier_mult,
            decoder_chroma_rotation: decoder_params.chroma_rotation,
            decoder_chroma_offset: decoder_params.chroma_offset,
            decoder_nonlinear_highpass_limit_l: decoder_params.nonlinear.highpass_limit_l,
            decoder_nonlinear_highpass_limit_h: decoder_params.nonlinear.highpass_limit_h,
            decoder_nonlinear_exp_scaling: decoder_params.nonlinear.exp_scaling,
            decoder_nonlinear_scaling_1: decoder_params.nonlinear.scaling_1,
            decoder_nonlinear_scaling_2: decoder_params.nonlinear.scaling_2,
            decoder_nonlinear_logistic: decoder_params
                .nonlinear
                .logistic
                .as_ref()
                .map(|value| (value.mid, value.rate)),
            decoder_nonlinear_static_factor: decoder_params.nonlinear.static_factor,
            rf_chroma_heterodyne,
            rf_fsc_wave,

            dod_threshold_p: request.dod_threshold_p,
            dod_threshold_a: request.dod_threshold_a,
            dod_hysteresis: request.dod_hysteresis,

            rf_disable_comb,
            rf_disable_right_hsync: request.rf_disable_right_hsync,
            rf_fallback_vsync,
            rf_field_order_confidence: request.rf_field_order_confidence,
            rf_saved_levels: request.rf_saved_levels,
            rf_y_comb,
            rf_write_chroma,
            chroma_output,
            rf_skip_hsync_refine: request.rf_skip_hsync_refine,
            rf_export_raw_tbc: request.rf_export_raw_tbc,
            rf_ire0_adjust: request.rf_ire0_adjust,
            rf_relaxed_line0: request.rf_relaxed_line0,
            rf_detect_chroma_track_phase: request.rf_detect_chroma_track_phase,
            rf_enable_color_killer: request.rf_enable_color_killer,
            rf_disable_burst_hsync,
            rf_disable_phase_correction: request.rf_disable_phase_correction,
            rf_secam_carrier_servo,
            secam_lo_trim: request.secam_lo_trim,
            cti_mix,
            cti_width: request.cti_width,

            chroma_burst_block_fft_gain,
            chroma_filter_deemphasis: chroma_filter_deemphasis.map(store_sos_filter),
            chroma_filter_final: store_sos_filter(chroma_filter_final),
            chroma_fft_filter,
            secam_under_filter: secam_under_filter.map(store_sos_filter),

            video_rf_filter: store_real_filter(video_rf_filter),
            video_notch_filter: video_notch_filter.map(store_real_filter),
            video_env_post_filter: store_sos_filter(video_env_post_filter),
            video_rf_top_fft_gain,
            video_high_boost_value,
            video_disable_diff_demod: request.video_disable_diff_demod,
            video_chroma_trap,
            video_filter: store_complex_filter(video_filter),
            video_nl_amplitude_lpf: store_sos_filter(video_nl_amplitude_lpf),
            video_nl_high_pass_f: video_nl_high_pass_f.map(store_complex_filter),
            video_nldeemp_enabled: request.video_nldeemp_enabled,
            video_subdeemp_enabled,
            video_sub_deemphasis_deviation,
            video_fsc_notch: video_fsc_notch.map(store_sos_filter),
            video05_filter: store_complex_filter(video05_filter),

            fft_block_inverse_f32,
            fft_block_r2c_f32,
            fft_block_c2r_f32,
            fft_field_forward_f32,

            black_ire,
            track_phase,
            wow_level_adjust_smoothing,
            wow_interpolation_method: request.wow_interpolation_method,
            sinc_lut: SincLut::new(),
            do_dod: request.do_dod,
            level_adjust: request.level_adjust,
            _private: (),
        })
    }

    pub(crate) const CHROMA_AFC_POWER_THRESHOLD: f64 = 1.0 / 3.0;
    // 65-tap FIR low-pass introduces a 32-sample group delay.
    pub(crate) const VIDEO05_FILTER_OFFSET: usize = 32;

    #[inline]
    pub(crate) fn freq_hz(&self) -> f64 {
        self.freq * 1_000_000.0
    }

    #[inline]
    pub(crate) fn linelen(&self) -> usize {
        (self.freq_hz() / (1_000_000.0 / self.sys_line_period)).round_ties_even() as usize
    }

    #[inline]
    pub(crate) fn chroma_offset(&self) -> isize {
        (self.decoder_chroma_offset * (self.freq / 40.0)) as isize
    }

    #[inline]
    pub(crate) fn chroma_afc_fh(&self) -> f64 {
        self.sys_fps * self.sys_frame_lines.line_count() as f64
    }

    #[inline]
    pub(crate) fn chroma_afc_band_tolerance(&self) -> (f64, f64) {
        let color_under = self.decoder_color_under_carrier;
        let percent = 200.0 * self.chroma_afc_fh() / color_under;
        ((100.0 - percent) / 100.0, (100.0 + percent) / 100.0)
    }

    #[inline]
    pub(crate) fn chroma_afc_enabled(&self) -> bool {
        !self.chroma_afc_narrowband.is_empty()
    }

    /// Output samples per second the TBC produces: `outlinelen` per line
    /// period.
    #[inline]
    pub(crate) fn tbc_sample_rate(&self) -> f64 {
        self.sys_outlinelen as f64 * 1e6 / self.sys_line_period
    }

    /// Output samples of the longest field: the length of the heterodyne
    /// tables.
    #[inline]
    pub(crate) fn fieldlen(&self) -> usize {
        self.sys_outlinelen * self.sys_field_lines[0].max(self.sys_field_lines[1]) as usize
    }

    /// Whether the chroma output carries an FM carrier whose phase continues
    /// from one field into the next (ME-SECAM). Decoders that reach the same
    /// field with different field counts or carrier servo histories then write
    /// different chroma samples for it.
    #[inline]
    pub fn chroma_phase_continuous(&self) -> bool {
        self.chroma_output && self.decoder_chroma_conversion_lo.is_some()
    }

    /// Input samples read for a field: the longer field and the lines
    /// around it the field decode reaches, before its vertical interval and
    /// past its last line.
    #[inline]
    pub fn readlen(&self) -> usize {
        const MARGIN_LINES: usize = 87;
        let field_lines = self.sys_field_lines[0].max(self.sys_field_lines[1]) as usize;
        self.linelen() * (field_lines + MARGIN_LINES)
    }

    #[inline]
    pub(crate) fn usable_blocksize(&self) -> usize {
        BLOCKSIZE - (BLOCKCUT + BLOCKCUT_END)
    }

    #[inline]
    pub(crate) fn bytes_per_field(&self) -> u64 {
        (self.freq_hz() / (self.sys_fps * 2.0)) as u64 + 1
    }

    /// Input samples spanned by one field, used to translate a field distance
    /// into a seek offset for multithreaded decoding.
    #[inline]
    pub fn samples_per_field(&self) -> u64 {
        self.bytes_per_field().max(1)
    }

    /// Whether the decode has chroma to write: a colour-under format.
    #[inline]
    pub fn decodes_chroma(&self) -> bool {
        self.rf_write_chroma
    }
}

/// Smallest 5-smooth length at or above `n`, which the FFT runs with only
/// small radices.
pub(crate) fn next_fast_len(n: usize) -> usize {
    (n.max(1)..)
        .find(|&candidate| {
            let mut rest = candidate;
            for radix in [2, 3, 5] {
                while rest % radix == 0 {
                    rest /= radix;
                }
            }
            rest == 1
        })
        .unwrap()
}

fn gen_wave_at_frequency(
    frequency: f64,
    sample_frequency: f64,
    num_samples: usize,
) -> Vec<(f32, f32)> {
    use std::f64::consts::TAU;
    let angle_step = TAU * frequency / sample_frequency;
    (0..num_samples)
        .map(|i| {
            let (sin, cos) = (angle_step * i as f64).sin_cos();
            (sin as f32, cos as f32)
        })
        .collect()
}

/// Checks the profile's parameters the decoder takes for granted and no
/// filter design covers: the line geometry, and the nonlinear deemphasis
/// and chroma AFC domains, which panic, hang or write garbage otherwise.
fn check_profile(
    sys_params: &SysParams,
    decoder_params: &DecoderParams,
    decode_options: &DecodeOptions,
) -> Result<()> {
    if sys_params.line_period.is_nan() || sys_params.line_period <= 0.0 {
        bail!("line_period must be positive");
    }
    if sys_params.fps.is_nan() || sys_params.fps <= 0.0 {
        bail!("fps must be positive");
    }
    if sys_params.fsc_mhz.is_nan() || sys_params.fsc_mhz <= 0.0 {
        bail!("fsc_mhz must be positive");
    }
    if sys_params.hz_ire.is_nan() || sys_params.hz_ire <= 0.0 {
        bail!("hz_ire must be positive");
    }
    if sys_params.outlinelen == 0 {
        bail!("outlinelen must be at least one sample");
    }
    let frame_lines = sys_params.frame_lines.line_count() as i64;
    let mut field_lines = sys_params.field_lines;
    field_lines.sort_unstable();
    if field_lines != [(frame_lines - 1) / 2, (frame_lines + 1) / 2] {
        bail!(
            "field_lines {:?} are not the two halves of the {frame_lines} frame lines",
            sys_params.field_lines
        );
    }
    let within_line = |what: &str, [start, end]: [f64; 2]| -> Result<()> {
        if start.is_nan()
            || end.is_nan()
            || start < 0.0
            || start >= end
            || end > sys_params.line_period
        {
            bail!(
                "{what} {start} to {end} us does not lie within the {} us line",
                sys_params.line_period
            );
        }
        Ok(())
    };
    within_line("color_burst_us", sys_params.color_burst_us)?;
    within_line("active_video_us", sys_params.active_video_us)?;

    let nonlinear = &decoder_params.nonlinear;
    if nonlinear.highpass_limit_l > nonlinear.highpass_limit_h {
        bail!(
            "nonlinear.highpass_limit_l of {} Hz is above highpass_limit_h of {} Hz",
            nonlinear.highpass_limit_l,
            nonlinear.highpass_limit_h
        );
    }
    // The exponent of the sub-deemphasis' amplitude root; the fast power
    // function overflows its exponent on zero amplitude past 1.
    let exp_scaling = nonlinear.exp_scaling;
    if exp_scaling.is_nan() || exp_scaling <= 0.0 || exp_scaling > 1.0 {
        bail!(
            "nonlinear.exp_scaling of {exp_scaling} is not within 0 (exclusive) to 1"
        );
    }
    // The chroma AFC fine-tunes the carrier in steps of this fraction of
    // the line frequency, stepping until it is within one of the nominal
    // carrier.
    let fh_ratio = decode_options.chroma_afc_fine_tune_fh_ratio;
    if fh_ratio.is_nan() || fh_ratio <= 0.0 {
        bail!(
            "chroma_afc_fine_tune_fh_ratio of {} is not positive",
            decode_options.chroma_afc_fine_tune_fh_ratio
        );
    }
    Ok(())
}

/// The color burst window of `color_burst_us` in output samples at
/// `tbc_sample_rate` (see [`DecoderSpec::burst_area`]).
fn burst_area(color_burst_us: [f64; 2], tbc_sample_rate: f64) -> (usize, usize) {
    let to_outpx = |us: f64| us * tbc_sample_rate / 1e6;
    let start = (to_outpx(color_burst_us[0]).floor() as isize - 4).max(0) as usize;
    let end = to_outpx(color_burst_us[1]).ceil() as usize + 8;
    (start, end - (end - start) % 4)
}

/// A frequency in hertz as megahertz with hertz resolution, for messages.
fn mhz(hz: f64) -> f64 {
    hz.round() / 1e6
}

/// Critical frequencies of a digital filter in half-cycles per sample, from
/// the `cutoffs_hz` of `what` (one, or a band's low and high edge) on a
/// signal sampled at `sample_rate_hz`. Each must lie above zero and below
/// the Nyquist frequency, and a band's low edge below its high edge, for
/// the filter to exist.
fn digital_cutoffs(what: &str, cutoffs_hz: &[f64], sample_rate_hz: f64) -> Result<Vec<f64>> {
    let nyquist_hz = sample_rate_hz / 2.0;
    for &cutoff in cutoffs_hz {
        if cutoff.is_nan() || cutoff <= 0.0 {
            bail!("{what}: a cutoff of {} MHz is not above 0", mhz(cutoff));
        }
        if cutoff >= nyquist_hz {
            bail!(
                "{what}: a cutoff of {} MHz is not below the Nyquist frequency of {} MHz at the {} MHz sample rate",
                mhz(cutoff),
                mhz(nyquist_hz),
                mhz(sample_rate_hz)
            );
        }
    }
    if let [low, high] = cutoffs_hz {
        if low >= high {
            bail!(
                "{what}: the band's low edge of {} MHz is not below its high edge of {} MHz",
                mhz(*low),
                mhz(*high)
            );
        }
    }
    Ok(cutoffs_hz
        .iter()
        .map(|cutoff| cutoff / nyquist_hz)
        .collect())
}

/// Butterworth filter of `order` with the `cutoffs_hz` of `what` (one for a
/// low- or high-pass, the band edges for a band-pass) on a signal sampled at
/// `sample_rate_hz`, as second-order sections.
fn butter_sos(
    what: &str,
    order: usize,
    cutoffs_hz: &[f64],
    sample_rate_hz: f64,
    band_type: FilterBandType,
) -> Result<Vec<Sos<f64>>> {
    use sci_rs::signal::filter::design::{butter_dyn, DigitalFilter, FilterOutputType};

    let wn = butter_cutoffs(what, order, cutoffs_hz, sample_rate_hz)?;
    match butter_dyn::<f64>(
        order,
        wn,
        Some(band_type),
        Some(false),
        Some(FilterOutputType::Sos),
        None,
    ) {
        DigitalFilter::Sos(sos) => Ok(sos.sos),
        _ => unreachable!("the Butterworth design returns the representation asked for"),
    }
}

/// [`butter_sos`] as (b, a) taps.
fn butter_ba(
    what: &str,
    order: usize,
    cutoffs_hz: &[f64],
    sample_rate_hz: f64,
    band_type: FilterBandType,
) -> Result<(Vec<f64>, Vec<f64>)> {
    use sci_rs::signal::filter::design::{butter_dyn, DigitalFilter, FilterOutputType};

    let wn = butter_cutoffs(what, order, cutoffs_hz, sample_rate_hz)?;
    match butter_dyn::<f64>(
        order,
        wn,
        Some(band_type),
        Some(false),
        Some(FilterOutputType::Ba),
        None,
    ) {
        DigitalFilter::Ba(mut ba) => {
            while ba.b.len() > 1
                && ba.a.len() > 1
                && ba.b.last().is_some_and(|value| value.abs() <= f64::EPSILON)
                && ba.a.last().is_some_and(|value| value.abs() <= f64::EPSILON)
            {
                ba.b.pop();
                ba.a.pop();
            }
            Ok((ba.b, ba.a))
        }
        _ => unreachable!("the Butterworth design returns the representation asked for"),
    }
}

/// The Butterworth designs' checked inputs: an order of at least one and the
/// digital cutoffs.
fn butter_cutoffs(
    what: &str,
    order: usize,
    cutoffs_hz: &[f64],
    sample_rate_hz: f64,
) -> Result<Vec<f64>> {
    if order == 0 {
        bail!("{what}: a filter order of 0 designs no filter");
    }
    digital_cutoffs(what, cutoffs_hz, sample_rate_hz)
}

/// Notch of quality factor `q` at the `freq_hz` of `what` on a signal
/// sampled at `sample_rate_hz`, as (b, a) taps.
fn iirnotch(what: &str, freq_hz: f64, q: f64, sample_rate_hz: f64) -> Result<([f64; 3], [f64; 3])> {
    let w0 = digital_cutoffs(what, &[freq_hz], sample_rate_hz)?[0];
    if q.is_nan() || q <= 0.0 {
        bail!("{what}: a notch Q of {q} is not positive");
    }

    let w0_radians = std::f64::consts::PI * w0;
    let bandwidth = std::f64::consts::PI * (w0 / q);
    let beta = (bandwidth / 2.0).tan();
    let gain = 1.0 / (1.0 + beta);
    let cos_w0 = w0_radians.cos();

    Ok((
        [gain, -2.0 * gain * cos_w0, gain],
        [1.0, -2.0 * gain * cos_w0, (2.0 * gain) - 1.0],
    ))
}

/// Multiply two polynomials given in ascending-power coefficient order.
fn poly_mul(p: &[f64], q: &[f64]) -> Vec<f64> {
    let mut r = vec![0.0; p.len() + q.len() - 1];
    for (i, &pi) in p.iter().enumerate() {
        for (j, &qj) in q.iter().enumerate() {
            r[i + j] += pi * qj;
        }
    }
    r
}

/// Raise a polynomial (ascending-power coefficients) to an integer power.
fn poly_pow(p: &[f64], n: usize) -> Vec<f64> {
    let mut r = vec![1.0];
    for _ in 0..n {
        r = poly_mul(&r, p);
    }
    r
}

/// Accumulate `scale * p` into `acc`, both in ascending-power coefficient order.
fn poly_add_scaled(acc: &mut Vec<f64>, p: &[f64], scale: f64) {
    if p.len() > acc.len() {
        acc.resize(p.len(), 0.0);
    }
    for (i, &pi) in p.iter().enumerate() {
        acc[i] += scale * pi;
    }
}

/// Normalize numerator/denominator coefficients of a transfer function.
fn normalize(b: &[f64], a: &[f64]) -> (Vec<f64>, Vec<f64>) {
    // Trim leading zeros in denominator, leave at least one.
    let den_start = a
        .iter()
        .position(|&v| v != 0.0)
        .unwrap_or(a.len().saturating_sub(1));
    let den = &a[den_start..];

    // Normalize transfer function.
    let a0 = den[0];
    let num: Vec<f64> = b.iter().map(|&v| v / a0).collect();
    let den: Vec<f64> = den.iter().map(|&v| v / a0).collect();

    // Trim leading near-zero numerator coefficients, leaving at least one.
    let num_start = num
        .iter()
        .position(|&v| v.abs() > 1e-14)
        .unwrap_or(num.len().saturating_sub(1));
    let num = num[num_start..].to_vec();

    (num, den)
}

/// Transform a lowpass analog prototype to a different cutoff frequency (BA form).
fn lp2lp(b: &[f64], a: &[f64], wo: f64) -> (Vec<f64>, Vec<f64>) {
    let d = a.len();
    let n = b.len();
    let m = d.max(n);
    // pwo = wo ** arange(M - 1, -1, -1)
    let pwo: Vec<f64> = (0..m).map(|i| wo.powi((m - 1 - i) as i32)).collect();
    let start1 = n.saturating_sub(d);
    let start2 = d.saturating_sub(n);
    let b_new: Vec<f64> = b
        .iter()
        .enumerate()
        .map(|(i, &bv)| bv * pwo[start1] / pwo[start2 + i])
        .collect();
    let a_new: Vec<f64> = a
        .iter()
        .enumerate()
        .map(|(i, &av)| av * pwo[start1] / pwo[start1 + i])
        .collect();
    normalize(&b_new, &a_new)
}

/// Calculate a digital IIR filter from an analog transfer function (BA form)
/// using the bilinear transform.
fn bilinear(b: &[f64], a: &[f64], fs: f64) -> (Vec<f64>, Vec<f64>) {
    // remove leading zeros
    let b: Vec<f64> = {
        let start = b.iter().position(|&v| v != 0.0).unwrap_or(b.len());
        b[start..].to_vec()
    };
    let a: Vec<f64> = {
        let start = a.iter().position(|&v| v != 0.0).unwrap_or(a.len());
        a[start..].to_vec()
    };

    // Splitting the factor fs*2 between numerator and denominator reduces the chance of
    // numeric overflow for large fs and large N.
    let fac = (fs * 2.0).sqrt();
    // zp1 = (z + 1) / fac, zm1 = (z - 1) * fac (ascending-power coefficients)
    let zp1 = [1.0 / fac, 1.0 / fac];
    let zm1 = [-fac, fac];

    let n = a.len().max(b.len()) - 1;
    let mut numerator: Vec<f64> = vec![0.0];
    for (q, &b_) in b.iter().rev().enumerate() {
        let term = poly_mul(&poly_pow(&zp1, n - q), &poly_pow(&zm1, q));
        poly_add_scaled(&mut numerator, &term, b_);
    }
    let mut denominator: Vec<f64> = vec![0.0];
    for (p, &a_) in a.iter().rev().enumerate() {
        let term = poly_mul(&poly_pow(&zp1, n - p), &poly_pow(&zm1, p));
        poly_add_scaled(&mut denominator, &term, a_);
    }

    // Polynomial helpers use ascending powers; BA transfer functions use descending powers.
    numerator.reverse();
    denominator.reverse();
    normalize(&numerator, &denominator)
}

/// Convert an analog prototype filter to a digital BA transfer function at
/// the critical frequency `wn`, in half-cycles per sample within (0, 1).
fn biquad_transform(b: &[f64], a: &[f64], wn: f64) -> (Vec<f64>, Vec<f64>) {
    assert!(
        0.0 < wn && wn < 1.0,
        "the critical frequency was checked against the signal"
    );
    let fs = 2.0;
    let warped = 2.0 * fs * (std::f64::consts::PI * wn / fs).tan();

    // Shift frequency
    let (b, a) = lp2lp(b, a, warped);
    // Find discrete equivalent
    bilinear(&b, &a, fs)
}

/// Digital biquad peaking filter of `db_gain` at the `freq_hz` of `what`
/// over `bandwidth_hz`, on a signal sampled at `sample_rate_hz` (BA output).
fn peaking(
    what: &str,
    freq_hz: f64,
    bandwidth_hz: f64,
    db_gain: f64,
    sample_rate_hz: f64,
) -> Result<(Vec<f64>, Vec<f64>)> {
    let wn = digital_cutoffs(what, &[freq_hz], sample_rate_hz)?[0];
    if bandwidth_hz.is_nan() || bandwidth_hz <= 0.0 {
        bail!(
            "{what}: a bandwidth of {} MHz is not above 0",
            mhz(bandwidth_hz)
        );
    }
    let bw = bandwidth_hz / (sample_rate_hz / 2.0);
    let q = 1.0 / (2.0 * (std::f64::consts::LN_2 / 2.0 * bw).sinh());

    let (az, ap) = {
        let a = 10.0_f64.powf(db_gain / 20.0);
        if db_gain > 0.0 {
            (a, 1.0) // boost
        } else {
            (1.0, a) // cut
        }
    };

    // H(s) = (s**2 + s*(Az/Q) + 1) / (s**2 + s/(Ap*Q) + 1)
    let b = [1.0, az / q, 1.0];
    let a = [1.0, 1.0 / (ap * q), 1.0];

    Ok(biquad_transform(&b, &a, wn))
}

/// Biquad shelving filter of `dbgain` with its midpoint at the `f0` Hz of
/// `what` and the quality factor `q`, for a signal sampled at `fs` Hz, as
/// (b, a) taps.
fn gen_shelf(
    what: &str,
    f0: f64,
    dbgain: f64,
    shelf_type: ShelfKind,
    fs: f64,
    q: f64,
) -> Result<([f64; 3], [f64; 3])> {
    digital_cutoffs(what, &[f0], fs)?;
    if q.is_nan() || q <= 0.0 {
        bail!("{what}: a Q of {q} is not positive");
    }
    let a = 10.0_f64.powf(dbgain / 40.0);
    let w0 = std::f64::consts::TAU * (f0 / fs);
    let alpha = w0.sin() / (2.0 * q);
    let cosw0 = w0.cos();
    let asquared = a.sqrt();

    Ok(match shelf_type {
        ShelfKind::Low => (
            [
                a * ((a + 1.0) - (a - 1.0) * cosw0 + 2.0 * asquared * alpha),
                2.0 * a * ((a - 1.0) - (a + 1.0) * cosw0),
                a * ((a + 1.0) - (a - 1.0) * cosw0 - 2.0 * asquared * alpha),
            ],
            [
                (a + 1.0) + (a - 1.0) * cosw0 + 2.0 * asquared * alpha,
                -2.0 * ((a - 1.0) + (a + 1.0) * cosw0),
                (a + 1.0) + (a - 1.0) * cosw0 - 2.0 * asquared * alpha,
            ],
        ),
        ShelfKind::High => (
            [
                a * ((a + 1.0) + (a - 1.0) * cosw0 + 2.0 * asquared * alpha),
                -2.0 * a * ((a - 1.0) + (a + 1.0) * cosw0),
                a * ((a + 1.0) + (a - 1.0) * cosw0 - 2.0 * asquared * alpha),
            ],
            [
                (a + 1.0) - (a - 1.0) * cosw0 + 2.0 * asquared * alpha,
                2.0 * ((a - 1.0) - (a + 1.0) * cosw0),
                (a + 1.0) - (a - 1.0) * cosw0 - 2.0 * asquared * alpha,
            ],
        ),
    })
}

/// Butterworth order and natural frequency in Hz for a digital low-pass of at
/// most `gpass` dB loss at the passband edge `wp` and at least `gstop` dB at
/// the stopband edge `ws`, both in Hz below the Nyquist frequency of `fs`.
fn buttord(wp: f64, ws: f64, gpass: f64, gstop: f64, fs: f64) -> (usize, f64) {
    assert!(
        0.0 < gpass && gpass < gstop,
        "the passband loss is positive and below the stopband loss"
    );

    // fs given: normalize edges to half-cycles/sample.
    let wp = 2.0 * wp / fs;
    let ws = 2.0 * ws / fs;

    // Pre-warp frequencies for digital filter design.
    let passb = (std::f64::consts::PI * wp / 2.0).tan();
    let stopb = (std::f64::consts::PI * ws / 2.0).tan();

    // filter_type == 1 (low): nat = stopb / passb
    let nat = (stopb / passb).abs();

    let gstop_lin = 10.0_f64.powf(0.1 * gstop.abs());
    let gpass_lin = 10.0_f64.powf(0.1 * gpass.abs());
    let ord = (((gstop_lin - 1.0) / (gpass_lin - 1.0)).log10() / (2.0 * nat.log10())).ceil();
    let ord = ord as usize;

    // Find the Butterworth natural frequency WN (the "3dB" frequency)
    // to give exactly gpass at passb.
    let w0 = if ord == 0 {
        1.0
    } else {
        (gpass_lin - 1.0).powf(-1.0 / (2.0 * ord as f64))
    };

    // Convert this frequency back from lowpass prototype to the original
    // analog filter (filter_type 1: low).
    let wn = w0 * passb;

    // postprocess: digital filter with fs specified.
    let wn = wn.atan() * 2.0 / std::f64::consts::PI;
    let wn = wn * fs / 2.0;

    (ord, wn)
}

/// Butterworth order, capped at `order_limit`, and natural frequency in Hz
/// of the low-pass of 3 dB loss at `passband` and 30 dB at `stopband`, both
/// in Hz, on a signal sampled at `samp_rate` Hz. The edges are the `what`
/// the caller designs for, checked against the signal here.
fn design_filter(
    what: &str,
    samp_rate: f64,
    passband: f64,
    stopband: f64,
    order_limit: usize,
) -> Result<(usize, f64)> {
    digital_cutoffs(what, &[passband, stopband], samp_rate)?;
    let max_loss_passband = 3.0;
    let min_loss_stopband = 30.0;
    let (mut order, normal_cutoff) = buttord(
        passband,
        stopband,
        max_loss_passband,
        min_loss_stopband,
        samp_rate,
    );
    if order > order_limit {
        tracing::warn!(
            "Limiting order of the filter from {} to {}",
            order,
            order_limit
        );
        order = order_limit;
    }
    Ok((order, normal_cutoff))
}

/// Butterworth low-pass of 3 dB loss at the `cutoff` Hz of `what` and 30 dB
/// `transition_width` Hz above it, of at most `order_limit`, as second-order
/// sections.
fn iir_lowpass_sos(
    what: &str,
    samp_rate: f64,
    cutoff: f64,
    transition_width: f64,
    order_limit: usize,
) -> Result<Vec<Sos<f64>>> {
    let stopband = cutoff + transition_width;
    let (order, normal_cutoff) = design_filter(what, samp_rate, cutoff, stopband, order_limit)?;
    butter_sos(
        what,
        order,
        &[normal_cutoff],
        samp_rate,
        FilterBandType::Lowpass,
    )
}

/// Butterworth high-pass designed like [`iir_lowpass_sos`].
fn iir_highpass_sos(
    what: &str,
    samp_rate: f64,
    cutoff: f64,
    transition_width: f64,
    order_limit: usize,
) -> Result<Vec<Sos<f64>>> {
    let stopband = cutoff + transition_width;
    let (order, normal_cutoff) = design_filter(what, samp_rate, cutoff, stopband, order_limit)?;
    butter_sos(
        what,
        order,
        &[normal_cutoff],
        samp_rate,
        FilterBandType::Highpass,
    )
}

fn gen_video_lpf_supergauss(
    corner_freq: f64,
    order: usize,
    nyquist_hz: f64,
    block_len: usize,
) -> Vec<f64> {
    let output_len = block_len / 2 + 1;
    let exponent = (order * 2) as i32;
    let half_log_two = f64::ln(2.0) / 2.0;
    let scale = half_log_two.powf(1.0 / exponent as f64);

    (0..output_len)
        .map(|index| {
            let x = if output_len <= 1 {
                0.0
            } else {
                nyquist_hz * index as f64 / (output_len - 1) as f64
            };
            let normalized = (2.0 * x * scale) / corner_freq;
            (-2.0 * normalized.powi(exponent)).exp()
        })
        .collect()
}

fn gen_ramp_filter(
    boost_ramp: &BoostRampFilter,
    nyquist_freq_hz: f64,
    block_len: usize,
) -> Vec<f64> {
    let max_freq_hz = 20e6;
    let half_len = block_len / 2;
    let zero_ratio = ((boost_ramp.start_freq / nyquist_freq_hz) * half_len as f64) as usize;
    let zero_ratio = zero_ratio.min(half_len);
    let ramp_len = half_len.saturating_sub(zero_ratio);

    let mut ramp = vec![0.0f64; half_len];
    if ramp_len > 0 {
        let end_value = boost_ramp.rf_linear_20 * (nyquist_freq_hz / max_freq_hz);
        if ramp_len == 1 {
            ramp[zero_ratio] = boost_ramp.rf_linear_0;
        } else {
            for i in 0..ramp_len {
                let t = i as f64 / (ramp_len - 1) as f64;
                ramp[zero_ratio + i] =
                    boost_ramp.rf_linear_0 + (end_value - boost_ramp.rf_linear_0) * t;
            }
        }
    }

    let mut output = Vec::with_capacity(block_len);
    output.extend_from_slice(&ramp);
    output.extend(ramp.iter().rev().copied());
    output
}

fn sosfiltfft(sos: &[Sos<f64>], block_len: usize) -> Vec<Complex64> {
    use std::f64::consts::TAU;

    let mut output = Vec::with_capacity(block_len);

    for k in 0..block_len {
        let omega = TAU * k as f64 / block_len as f64;
        let z1 = Complex64::new(omega.cos(), -omega.sin());
        let z2 = z1 * z1;
        let mut response = Complex64::new(1.0, 0.0);

        for section in sos {
            let [b0, b1, b2] = section.b;
            let [a0, a1, a2] = section.a;

            let numerator = Complex64::new(b0, 0.0) + z1 * b1 + z2 * b2;
            let denominator = Complex64::new(a0, 0.0) + z1 * a1 + z2 * a2;
            response *= numerator / denominator;
        }

        output.push(response);
    }

    output
}

fn filtfft(b: &[f64], a: &[f64], block_len: usize, whole: bool) -> Vec<Complex64> {
    use std::f64::consts::{PI, TAU};

    assert!(!a.is_empty());
    assert!(!b.is_empty());

    let output_len = if whole {
        block_len
    } else {
        (block_len / 2) + 1
    };
    let mut output = Vec::with_capacity(output_len);

    for k in 0..output_len {
        let omega = if whole {
            TAU * k as f64 / block_len as f64
        } else if output_len > 1 {
            PI * k as f64 / (output_len - 1) as f64
        } else {
            0.0
        };
        let z = Complex64::new(omega.cos(), -omega.sin());
        let mut z_power = Complex64::new(1.0, 0.0);
        let mut numerator = Complex64::new(0.0, 0.0);
        for coefficient in b {
            numerator += z_power * *coefficient;
            z_power *= z;
        }

        z_power = Complex64::new(1.0, 0.0);
        let mut denominator = Complex64::new(0.0, 0.0);
        for coefficient in a {
            denominator += z_power * *coefficient;
            z_power *= z;
        }

        output.push(numerator / denominator);
    }

    output
}

fn ba_to_vec(pair: ([f64; 3], [f64; 3])) -> (Vec<f64>, Vec<f64>) {
    (pair.0.to_vec(), pair.1.to_vec())
}

/// Wrap a single biquad (3-tap BA) as a one-section SOS cascade, so it runs
/// through the shared `sosfiltfilt`/`sosfilt` path like the other filters.
fn biquad_sos(pair: ([f64; 3], [f64; 3])) -> Vec<Sos<f64>> {
    vec![Sos::new(pair.0, pair.1)]
}

/// Wrap a single biquad given as length-3 BA coefficient vectors (e.g. the
/// `peaking`/`bilinear` designers) as a one-section SOS cascade.
fn biquad_sos_vec(ba: (Vec<f64>, Vec<f64>)) -> Vec<Sos<f64>> {
    let (b, a) = ba;
    debug_assert_eq!(b.len(), 3);
    debug_assert_eq!(a.len(), 3);
    vec![Sos::new([b[0], b[1], b[2]], [a[0], a[1], a[2]])]
}

fn abs_complex(values: Vec<Complex64>) -> Vec<f64> {
    values.into_iter().map(|value| value.norm()).collect()
}

fn multiply<A, B, O>(a: &[A], b: &[B]) -> Vec<O>
where
    A: Copy + std::ops::Mul<B, Output = O>,
    B: Copy,
{
    assert_eq!(b.len(), a.len(), "multiply length mismatch");
    a.iter().zip(b).map(|(&x, &y)| x * y).collect()
}

fn gen_video_main_deemp_fft(
    deemph: &DeemphasisParams,
    freq_hz: f64,
    block_len: usize,
) -> Result<Vec<Complex64>> {
    let (ataps, btaps) = gen_shelf(
        "deemph.mid",
        deemph.mid,
        deemph.gain,
        ShelfKind::High,
        freq_hz,
        deemph.q,
    )?;
    Ok(filtfft(&btaps, &ataps, block_len, false))
}

fn gen_video_lpf(
    corner_freq: f64,
    order: usize,
    sample_rate_hz: f64,
    block_len: usize,
) -> Result<Vec<f64>> {
    let sos = butter_sos(
        "video_lpf_freq",
        order,
        &[corner_freq],
        sample_rate_hz,
        FilterBandType::Lowpass,
    )?;
    let mut full = abs_complex(sosfiltfft(&sos, block_len));
    full.truncate(block_len / 2 + 1);
    Ok(full)
}

fn gen_nonlinear_bandpass(
    nonlinear: &NonlinearParams,
    sample_rate_hz: f64,
    block_len: usize,
) -> Result<Vec<Complex64>> {
    let (b, a) = if let Some(upper_freq) = nonlinear.bandpass_upper {
        butter_ba(
            "nonlinear.highpass_freq and bandpass_upper",
            nonlinear.bandpass_order,
            &[nonlinear.highpass_freq, upper_freq],
            sample_rate_hz,
            FilterBandType::Bandpass,
        )?
    } else {
        butter_ba(
            "nonlinear.highpass_freq",
            nonlinear.bandpass_order,
            &[nonlinear.highpass_freq],
            sample_rate_hz,
            FilterBandType::Highpass,
        )?
    };
    Ok(filtfft(&b, &a, block_len, false))
}

fn gen_fft_notch(
    what: &str,
    notch_freq: f64,
    notch_q: f64,
    sample_rate_hz: f64,
    block_len: usize,
) -> Result<Vec<Complex64>> {
    let (b, a) = ba_to_vec(iirnotch(what, notch_freq, notch_q, sample_rate_hz)?);
    Ok(filtfft(&b, &a, block_len, true))
}

/// Half-spectrum response of an analog transfer function of first-order real
/// zeros and poles given as time constants, H(s) = prod(1 + s tz) / prod(1 +
/// s tp). Evaluated on the analog frequency axis rather than through a
/// bilinear transform, so there is no warping towards Nyquist and the
/// response is the same at any sample rate.
fn gen_analog_filter(
    zeros_tau: &[f64],
    poles_tau: &[f64],
    freq_hz: f64,
    block_len: usize,
) -> Vec<Complex64> {
    use std::f64::consts::TAU;

    let output_len = block_len / 2 + 1;
    (0..output_len)
        .map(|k| {
            let f = if output_len > 1 {
                freq_hz / 2.0 * k as f64 / (output_len - 1) as f64
            } else {
                0.0
            };
            let s = Complex64::new(0.0, TAU * f);
            let factor = |tau: &f64| 1.0 + s * *tau;
            zeros_tau.iter().map(factor).product::<Complex64>()
                / poles_tau.iter().map(factor).product::<Complex64>()
        })
        .collect()
}

fn gen_custom_video_filters(
    filter_list: &[VideoLumaFilter],
    freq_hz: f64,
    block_len: usize,
) -> Result<Vec<Complex64>> {
    let mut ret = vec![Complex64::new(1.0, 0.0); block_len / 2 + 1];
    for filter in filter_list {
        match filter {
            VideoLumaFilter::Analog {
                zeros_tau,
                poles_tau,
            } => {
                let fft = gen_analog_filter(zeros_tau, poles_tau, freq_hz, block_len);
                ret = multiply(&ret, &fft);
            }
            VideoLumaFilter::Shelf {
                shelf_kind,
                gain,
                midfreq,
                q,
            } => {
                let (b, a) = ba_to_vec(gen_shelf(
                    "video_custom_luma_filters",
                    *midfreq,
                    *gain,
                    *shelf_kind,
                    freq_hz,
                    *q,
                )?);
                let fft = filtfft(&b, &a, block_len, false);
                ret = multiply(&ret, &fft);
            }
        }
    }
    Ok(ret)
}

fn firwin_lowpass(numtaps: usize, cutoff_norm_to_nyquist: f64) -> Vec<f64> {
    let m = (numtaps - 1) as f64 / 2.0;
    let fc = cutoff_norm_to_nyquist / 2.0;
    let mut taps = Vec::with_capacity(numtaps);
    for n in 0..numtaps {
        let x = n as f64 - m;
        let sinc = if x == 0.0 {
            2.0 * fc
        } else {
            (2.0 * std::f64::consts::PI * fc * x).sin() / (std::f64::consts::PI * x)
        };
        let window =
            0.54 - 0.46 * (2.0 * std::f64::consts::PI * n as f64 / (numtaps - 1) as f64).cos();
        taps.push(sinc * window);
    }
    let sum: f64 = taps.iter().sum();
    for tap in &mut taps {
        *tap /= sum;
    }
    taps
}
