//! Minimal FFI to the vendored libopus (see `build.rs`). Only what AudioBridge uses.

#![allow(non_camel_case_types)]

use std::os::raw::{c_int, c_uchar};

/// Opaque encoder state.
#[repr(C)]
pub struct OpusEncoder {
    _private: [u8; 0],
}

/// Opaque decoder state.
#[repr(C)]
pub struct OpusDecoder {
    _private: [u8; 0],
}

pub type opus_int32 = i32;

pub const OPUS_OK: c_int = 0;
pub const OPUS_APPLICATION_RESTRICTED_LOWDELAY: c_int = 2051;
pub const OPUS_SET_BITRATE_REQUEST: c_int = 4002;
pub const OPUS_SET_VBR_REQUEST: c_int = 4006;
pub const OPUS_SET_COMPLEXITY_REQUEST: c_int = 4010;
pub const OPUS_RESET_STATE: c_int = 4028;
pub const OPUS_SET_EXPERT_FRAME_DURATION_REQUEST: c_int = 4040;
pub const OPUS_FRAMESIZE_10_MS: c_int = 5003;

extern "C" {
    pub fn opus_encoder_create(
        fs: opus_int32,
        channels: c_int,
        application: c_int,
        error: *mut c_int,
    ) -> *mut OpusEncoder;
    pub fn opus_encode_float(
        st: *mut OpusEncoder,
        pcm: *const f32,
        frame_size: c_int,
        data: *mut c_uchar,
        max_data_bytes: opus_int32,
    ) -> opus_int32;
    pub fn opus_encoder_ctl(st: *mut OpusEncoder, request: c_int, ...) -> c_int;
    pub fn opus_encoder_destroy(st: *mut OpusEncoder);

    pub fn opus_decoder_create(fs: opus_int32, channels: c_int, error: *mut c_int) -> *mut OpusDecoder;
    pub fn opus_decode_float(
        st: *mut OpusDecoder,
        data: *const c_uchar,
        len: opus_int32,
        pcm: *mut f32,
        frame_size: c_int,
        decode_fec: c_int,
    ) -> c_int;
    pub fn opus_decoder_ctl(st: *mut OpusDecoder, request: c_int, ...) -> c_int;
    pub fn opus_decoder_destroy(st: *mut OpusDecoder);
}
