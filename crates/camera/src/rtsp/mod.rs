//! Serveur RTSP : met le flux H.264 de la caméra à disposition des lecteurs
//! vidéo et des enregistreurs du réseau (VLC, ffmpeg, Home Assistant,
//! Frigate, un NVR...).
//!
//! # Pourquoi RTSP en plus du WebSocket
//!
//! L'interface embarquée de la caméra diffuse déjà son flux par WebSocket
//! (voir `crate::api`), mais sous une forme qui n'appartient qu'à elle : des
//! images JPEG successives, lues par une page HTML écrite pour ça. Rien
//! d'autre ne sait l'ouvrir. RTSP, lui, est le protocole que *tous* les
//! outils vidéo savent lire : le même flux devient consultable depuis un
//! lecteur, enregistrable par un NVR, intégrable dans une domotique, sans
//! écrire une ligne de code pour chacun.
//!
//! # Découpage
//!
//! - [`stream`] : le flux partagé entre la boucle de capture et les sessions.
//! - [`server`] : l'acceptation des lecteurs et le dialogue RTSP.
//! - [`message`] : l'analyse et le formatage des messages RTSP.
//! - [`transport`] : la négociation du transport (TCP entrelacé ou UDP).
//! - [`sdp`] : la description du flux renvoyée à un `DESCRIBE`.
//! - [`rtp`] : l'empaquetage des frames en paquets RTP et les rapports RTCP.

mod message;
mod rtp;
mod sdp;
mod server;
mod stream;
mod transport;

pub use server::spawn;
pub use stream::RtspStream;
