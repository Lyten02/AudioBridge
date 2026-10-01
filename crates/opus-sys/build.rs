//! Builds the vendored libopus (float, no SIMD runtime detection, no DNN extensions) with `cc`.
//! Temporaries live on the stack (`USE_ALLOCA`), so encode/decode never touch the heap and are
//! safe to call from real-time audio threads.

use std::path::Path;

const CELT: &[&str] = &[
    "bands.c", "celt.c", "celt_encoder.c", "celt_decoder.c", "cwrs.c", "entcode.c", "entdec.c",
    "entenc.c", "kiss_fft.c", "laplace.c", "mathops.c", "mdct.c", "modes.c", "pitch.c",
    "celt_lpc.c", "quant_bands.c", "rate.c", "vq.c",
];

const SILK: &[&str] = &[
    "CNG.c", "code_signs.c", "init_decoder.c", "decode_core.c", "decode_frame.c",
    "decode_parameters.c", "decode_indices.c", "decode_pulses.c", "decoder_set_fs.c",
    "dec_API.c", "enc_API.c", "encode_indices.c", "encode_pulses.c", "gain_quant.c",
    "interpolate.c", "LP_variable_cutoff.c", "NLSF_decode.c", "NSQ.c", "NSQ_del_dec.c", "PLC.c",
    "shell_coder.c", "tables_gain.c", "tables_LTP.c", "tables_NLSF_CB_NB_MB.c",
    "tables_NLSF_CB_WB.c", "tables_other.c", "tables_pitch_lag.c",
    "tables_pulses_per_block.c", "VAD.c", "control_audio_bandwidth.c", "quant_LTP_gains.c",
    "VQ_WMat_EC.c", "HP_variable_cutoff.c", "NLSF_encode.c", "NLSF_VQ.c", "NLSF_unpack.c",
    "NLSF_del_dec_quant.c", "process_NLSFs.c", "stereo_LR_to_MS.c", "stereo_MS_to_LR.c",
    "check_control_input.c", "control_SNR.c", "init_encoder.c", "control_codec.c", "A2NLSF.c",
    "ana_filt_bank_1.c", "biquad_alt.c", "bwexpander_32.c", "bwexpander.c", "debug.c",
    "decode_pitch.c", "inner_prod_aligned.c", "lin2log.c", "log2lin.c",
    "LPC_analysis_filter.c", "LPC_inv_pred_gain.c", "table_LSF_cos.c", "NLSF2A.c",
    "NLSF_stabilize.c", "NLSF_VQ_weights_laroia.c", "pitch_est_tables.c", "resampler.c",
    "resampler_down2_3.c", "resampler_down2.c", "resampler_private_AR2.c",
    "resampler_private_down_FIR.c", "resampler_private_IIR_FIR.c",
    "resampler_private_up2_HQ.c", "resampler_rom.c", "sigm_Q15.c", "sort.c", "sum_sqr_shift.c",
    "stereo_decode_pred.c", "stereo_encode_pred.c", "stereo_find_predictor.c",
    "stereo_quant_pred.c", "LPC_fit.c",
];

const SILK_FLOAT: &[&str] = &[
    "apply_sine_window_FLP.c", "corrMatrix_FLP.c", "encode_frame_FLP.c", "find_LPC_FLP.c",
    "find_LTP_FLP.c", "find_pitch_lags_FLP.c", "find_pred_coefs_FLP.c",
    "LPC_analysis_filter_FLP.c", "LTP_analysis_filter_FLP.c", "LTP_scale_ctrl_FLP.c",
    "noise_shape_analysis_FLP.c", "process_gains_FLP.c", "regularize_correlations_FLP.c",
    "residual_energy_FLP.c", "warped_autocorrelation_FLP.c", "wrappers_FLP.c",
    "autocorrelation_FLP.c", "burg_modified_FLP.c", "bwexpander_FLP.c", "energy_FLP.c",
    "inner_product_FLP.c", "k2a_FLP.c", "LPC_inv_pred_gain_FLP.c", "pitch_analysis_core_FLP.c",
    "scale_copy_vector_FLP.c", "scale_vector_FLP.c", "schur_FLP.c", "sort_FLP.c",
];

const SRC: &[&str] = &[
    "opus.c", "opus_decoder.c", "opus_encoder.c", "extensions.c", "repacketizer.c",
    "analysis.c", "mlp.c", "mlp_data.c",
];

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("opus");
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let mut b = cc::Build::new();
    b.include(root.join("include"))
        .include(root.join("celt"))
        .include(root.join("silk"))
        .include(root.join("silk").join("float"))
        .include(root.join("src"))
        .define("OPUS_BUILD", None)
        .define("USE_ALLOCA", None)
        .define("HAVE_LRINTF", None)
        .define("HAVE_LRINT", None)
        // the codec is always built optimized (dev/test builds included)
        .opt_level(2)
        .warnings(false);
    if target_os != "windows" {
        b.define("HAVE_ALLOCA_H", None);
    }
    for f in CELT {
        b.file(root.join("celt").join(f));
    }
    for f in SILK {
        b.file(root.join("silk").join(f));
    }
    for f in SILK_FLOAT {
        b.file(root.join("silk").join("float").join(f));
    }
    for f in SRC {
        b.file(root.join("src").join(f));
    }
    b.compile("audiobridge_opus");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=opus");
}
