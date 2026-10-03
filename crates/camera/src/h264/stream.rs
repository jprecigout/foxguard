//! Le flux H.264 partagé entre la boucle de capture (qui le PRODUIT) et tous
//! ceux qui le CONSOMMENT.
//!
//! Ils sont trois, et c'est ce qui justifie que ce flux vive ici plutôt que
//! dans `crate::rtsp`, où il est né :
//!
//! - les **sessions RTSP**, pour les lecteurs du réseau (`crate::rtsp`) ;
//! - le **WebSocket H.264** des interfaces web, décodé par le navigateur
//!   (`crate::api`) ;
//! - les **enregistrements**, qui n'ont aucune raison de rester dix fois plus
//!   gros qu'il ne faut (`crate::capture::recording`).
//!
//! C'est la seule frontière entre la production et la consommation : la
//! boucle de capture ne sait rien du réseau, et aucun consommateur ne sait
//! rien de la caméra. L'encodage a lieu UNE fois quel que soit leur nombre.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::broadcast;

use super::{AccessUnit, ParameterSets};
use crate::util::MutexExt;

/// Nombre de frames encodées gardées en file pour les lecteurs en retard.
///
/// À 25 im/s, c'est un peu plus de deux secondes de marge. Un lecteur qui ne
/// suit pas au-delà est de toute façon perdu : plutôt que de faire grossir la
/// file (donc la latence de tout le monde), le canal l'informe qu'il a sauté
/// des frames, et la session lui demande une nouvelle image clé pour repartir
/// proprement (voir `crate::rtsp`).
const FRAME_QUEUE: usize = 64;

/// Un flux H.264 en direct, diffusé à zéro, un ou plusieurs consommateurs.
pub struct H264Stream {
    frames: broadcast::Sender<std::sync::Arc<AccessUnit>>,

    /// Derniers SPS/PPS vus, servis dans le SDP d'un `DESCRIBE`.
    ///
    /// Stockés ici et non dans l'encodeur parce que le serveur RTSP doit
    /// pouvoir les lire : l'encodeur, lui, vit dans le thread bloquant de la
    /// boucle de capture et n'est pas partageable.
    parameters: Mutex<Option<ParameterSets>>,

    /// Demande d'image clé en attente, posée par une session qui démarre (ou
    /// qui a décroché) et consommée par la boucle de capture.
    keyframe_requested: AtomicBool,
}

impl H264Stream {
    pub fn new() -> Self {
        let (frames, _) = broadcast::channel(FRAME_QUEUE);

        Self {
            frames,
            parameters: Mutex::new(None),
            keyframe_requested: AtomicBool::new(false),
        }
    }

    /// Vrai si au moins un consommateur est abonné — lecteur RTSP, onglet
    /// ouvert sur une interface web, ou enregistrement en cours.
    ///
    /// La boucle de capture s'en sert pour ne PAS encoder quand personne ne
    /// regarde : l'encodage H.264 logiciel est la dépense la plus lourde
    /// qu'on puisse ajouter au Raspberry Pi, et il serait absurde de la payer
    /// en permanence pour un flux que personne n'ouvre (voir
    /// `super::encoder`).
    pub fn has_viewers(&self) -> bool {
        self.frames.receiver_count() > 0
    }

    /// Publie une frame encodée à tous les consommateurs.
    ///
    /// Mémorise au passage les SPS/PPS d'une image clé, pour les prochains
    /// `DESCRIBE`.
    pub fn publish(&self, unit: AccessUnit, parameters: Option<&ParameterSets>) {
        if let Some(parameters) = parameters {
            let mut stored = self.parameters.lock_or_recover();

            if stored.as_ref() != Some(parameters) {
                *stored = Some(parameters.clone());
            }
        }

        // `send` échoue quand il n'y a aucun abonné : ce n'est pas une
        // erreur, c'est le cas normal quand aucun lecteur n'est connecté.
        let _ = self.frames.send(std::sync::Arc::new(unit));
    }

    /// Jeu de paramètres courant, pour le SDP.
    pub fn parameters(&self) -> Option<ParameterSets> {
        self.parameters.lock_or_recover().clone()
    }

    /// S'abonne au flux. Le simple fait de DÉTENIR l'abonnement rend
    /// [`Self::has_viewers`] vrai, et donc déclenche l'encodage — c'est ce
    /// qui fait qu'un consommateur n'a rien d'autre à faire pour être servi,
    /// et que l'encodage s'arrête de lui-même quand le dernier s'en va.
    pub fn subscribe(&self) -> broadcast::Receiver<std::sync::Arc<AccessUnit>> {
        self.frames.subscribe()
    }

    /// Demande que la prochaine frame encodée soit une image clé.
    ///
    /// Appelée quand un consommateur démarre (ou décroche) : sans image clé, son
    /// décodeur n'a aucun point d'entrée.
    pub fn request_keyframe(&self) {
        self.keyframe_requested.store(true, Ordering::Relaxed);
    }

    /// Consomme une éventuelle demande d'image clé (vrai une seule fois par
    /// demande).
    pub fn take_keyframe_request(&self) -> bool {
        self.keyframe_requested.swap(false, Ordering::Relaxed)
    }
}

impl Default for H264Stream {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn access_unit() -> AccessUnit {
        AccessUnit {
            nals: vec![vec![0x65, 1, 2, 3]],
            keyframe: true,
            rtp_timestamp: 0,
        }
    }

    fn parameters() -> ParameterSets {
        ParameterSets {
            sps: vec![0x67, 0x42, 0xC0, 0x1E],
            pps: vec![0x68, 0xCE],
        }
    }

    #[test]
    fn a_stream_without_a_viewer_does_not_ask_to_be_encoded() {
        // C'est l'économie la plus importante : pas de lecteur, pas
        // d'encodage H.264.
        let stream = H264Stream::new();
        assert!(!stream.has_viewers());
    }

    #[test]
    fn subscribing_makes_the_stream_want_frames() {
        let stream = H264Stream::new();
        let subscription = stream.subscribe();

        assert!(stream.has_viewers());

        drop(subscription);
        assert!(!stream.has_viewers());
    }

    #[test]
    fn publishing_without_a_viewer_is_not_an_error() {
        // Le cas normal au repos : la boucle de capture ne doit pas avoir à
        // s'en soucier.
        let stream = H264Stream::new();
        stream.publish(access_unit(), None);
    }

    #[tokio::test]
    async fn a_published_frame_reaches_every_subscriber() {
        let stream = H264Stream::new();
        let mut first = stream.subscribe();
        let mut second = stream.subscribe();

        stream.publish(access_unit(), None);

        assert_eq!(first.recv().await.expect("frame").nals, access_unit().nals);
        assert_eq!(second.recv().await.expect("frame").nals, access_unit().nals);
    }

    #[test]
    fn parameter_sets_are_remembered_for_the_next_describe() {
        let stream = H264Stream::new();
        assert_eq!(stream.parameters(), None);

        stream.publish(access_unit(), Some(&parameters()));

        assert_eq!(stream.parameters(), Some(parameters()));
    }

    #[test]
    fn a_keyframe_request_is_consumed_exactly_once() {
        // Deux sessions qui démarrent en même temps ne doivent pas provoquer
        // deux images clés successives.
        let stream = H264Stream::new();

        assert!(!stream.take_keyframe_request());

        stream.request_keyframe();
        stream.request_keyframe();

        assert!(stream.take_keyframe_request());
        assert!(!stream.take_keyframe_request());
    }
}
