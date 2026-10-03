//! Encodage H.264 du flux caméra : conversion vers le format d'entrée de
//! l'encodeur ([`i420`]), encodage proprement dit ([`encoder`]) et
//! distribution du résultat ([`stream`]).
//!
//! Ce module ne sait rien du réseau : il transforme des frames en unités
//! d'accès H.264 ([`AccessUnit`]) et les publie sur un [`H264Stream`]. Ce
//! sont ses abonnés qui décident quoi en faire — `crate::rtsp` les empaquette
//! en RTP pour les lecteurs du réseau, `crate::api` les pousse telles quelles
//! aux navigateurs, et `crate::capture::recording` les écrit sur disque.

mod encoder;
mod i420;
mod stream;

pub use encoder::{AccessUnit, H264Encoder, NAL_PPS, NAL_SPS, ParameterSets, nal_type};
pub use i420::I420Buffer;
pub use stream::H264Stream;
