//! Types de données partagés entre les différentes étapes du pipeline de
//! vision ([`super::object_detector`], [`super::face_detector`],
//! [`super::face_recognition`]).

use serde::{Deserialize, Serialize};

/// Une personne connue et son empreinte faciale de référence (512 floats,
/// normalisée L2). Plusieurs [`KnownPerson`] peuvent partager le même `name`
/// (plusieurs captures de référence pour une même personne, voir
/// `capture::known_faces::load_known_faces`) : la reconnaissance retient le
/// meilleur score.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnownPerson {
    pub name: String,
    pub embedding: Vec<f32>,
}

/// Une détection (personne, chat, chien, ou visage identifié) à afficher
/// sur le flux vidéo, en coordonnées de l'image d'origine.
#[derive(Debug, Clone)]
pub struct BoundingBox {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub label: String,
    pub confidence: f32,
}
