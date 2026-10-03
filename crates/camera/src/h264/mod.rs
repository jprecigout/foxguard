//! Encodage H.264 du flux caméra : conversion vers le format d'entrée de
//! l'encodeur ([`i420`]) et encodage proprement dit ([`encoder`]).
//!
//! Ce module ne sait rien du réseau : il transforme des frames en unités
//! d'accès H.264 ([`AccessUnit`]). C'est `crate::rtsp` qui les empaquette en
//! RTP et les distribue aux lecteurs.

mod encoder;
mod i420;

pub use encoder::{AccessUnit, H264Encoder, NAL_PPS, NAL_SPS, ParameterSets, nal_type};
pub use i420::I420Buffer;
