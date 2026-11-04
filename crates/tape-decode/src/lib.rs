#![cfg_attr(nightly_portable_simd, feature(portable_simd))]

mod decode;
mod optimized;
mod request;
mod spec;

pub use decode::{
    Decoder, DecoderMetadata, DropOuts, FieldInfoEntry, LumaOutput, VitsMetrics, WriteableField,
    BLOCKSIZE,
};
pub use request::{
    BoostBpf, BoostRampFilter, ColorSystem, DecodeOptions, DecodeProfile, DecodeRequest,
    DecoderParams, DeemphasisParams, FieldOrderAction, FmAudioChannels, Ire0Adjust, LineSystem,
    LogisticParams, NonlinearParams, NotchFilter, RfPeaking, ShelfKind, SysParams, VideoBpf,
    VideoEqBand, VideoEqParams, VideoLumaFilter, WowInterpolation,
};
pub use spec::DecoderSpec;
