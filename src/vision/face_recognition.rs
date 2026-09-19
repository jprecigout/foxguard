//! Extraction et comparaison d'empreintes faciales ONNX (ArcFace / MobileFaceNet).

use anyhow::Result;
use image::{RgbImage, imageops::FilterType};
use rayon::prelude::*;
use std::sync::Arc;
use tract_ndarray::prelude::*;
use tract_onnx::prelude::*;

use crate::config::DetectionConfig;

use super::model::load_onnx_model;
use super::types::KnownPerson;

/// Moteur d'extraction d'empreintes faciales ONNX (ArcFace / MobileFaceNet 112x112)
pub struct FaceEmbedder {
    model: Arc<TypedSimplePlan>,
    config: DetectionConfig,
}

impl FaceEmbedder {
    /// Charge le modèle ArcFace / MobileFaceNet à la taille d'entrée fixée
    /// par `config.input_face_size` (112x112 attendu).
    pub fn new(config: DetectionConfig) -> Result<Self> {
        let size = config.input_face_size;
        let model = load_onnx_model(&config.model_face_path, size, size)?;

        Ok(Self { model, config })
    }

    /// Extrait le vecteur d'empreinte (512 float) d'un découpage de visage
    pub fn extract_embedding(&self, face_crop: &RgbImage) -> Result<Vec<f32>> {
        let size = self.config.input_face_size as usize;
        let resized = image::imageops::resize(
            face_crop,
            self.config.input_face_size,
            self.config.input_face_size,
            FilterType::Triangle,
        );

        // Préparation du tenseur au format NCHW [1, 3, size, size]
        let mut tensor_data = Array4::<f32>::zeros((1, 3, size, size));

        for (x, y, pixel) in resized.enumerate_pixels() {
            let r = pixel[0] as f32;
            let g = pixel[1] as f32;
            let b = pixel[2] as f32;

            // Normalisation InsightFace/ArcFace : (x - 127.5) / 127.5
            //
            // IMPORTANT : contrairement à YuNet (modèle OpenCV natif, entraîné
            // en BGR), les modèles ArcFace/MobileFaceNet publiés par InsightFace
            // sont exportés avec un pré-traitement `cv2.dnn.blobFromImage(...,
            // swapRB=True)`, donc le réseau attend une image en ordre RGB, pas BGR.
            // Garder l'ordre B,G,R ici mélangeait les canaux et dégradait
            // fortement la qualité des embeddings (recon faciale non fonctionnelle).
            tensor_data[[0, 0, y as usize, x as usize]] = (r - 127.5) / 127.5; // R
            tensor_data[[0, 1, y as usize, x as usize]] = (g - 127.5) / 127.5; // G
            tensor_data[[0, 2, y as usize, x as usize]] = (b - 127.5) / 127.5; // B
        }

        let tensor: Tensor = tensor_data.into();
        let outputs = self.model.run(tvec!(tensor.into()))?;

        if outputs.is_empty() {
            return Err(anyhow::anyhow!("ArcFace n'a produit aucune sortie"));
        }

        let embedding_view = outputs[0].to_plain_array_view::<f32>()?;

        let embedding_slice = embedding_view.as_slice().ok_or_else(|| {
            anyhow::anyhow!("Impossible de lire la sortie ArcFace sous forme de slice")
        })?;

        if embedding_slice.is_empty() {
            return Err(anyhow::anyhow!("ArcFace a produit un embedding vide"));
        }

        let embedding = embedding_slice.to_vec();

        if embedding.len() != 512 {
            return Err(anyhow::anyhow!(
                "Embedding ArcFace inattendu : {} dimensions (attendu 512)",
                embedding.len()
            ));
        }

        // Normalisation L2

        let norm = embedding.iter().map(|x| x * x).sum::<f32>().sqrt();

        if !norm.is_finite() || norm < 1e-6 {
            return Err(anyhow::anyhow!(
                "Embedding ArcFace invalide : norme={:.6}",
                norm
            ));
        }

        let normalized: Vec<f32> = embedding.iter().map(|x| x / norm).collect();

        Ok(normalized)
    }

    /// Compare le vecteur extrait avec la base des personnes connues en parallèle via Rayon
    pub fn identify_person(
        detected_embedding: &[f32],
        known_people: &[KnownPerson],
        threshold: f32, // Ex: 0.60
    ) -> Option<(String, f32)> {
        known_people
            .par_iter()
            .map(|person| {
                let sim = cosine_similarity(detected_embedding, &person.embedding);
                (person.name.clone(), sim)
            })
            .filter(|(_, sim)| *sim >= threshold)
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
    }
}

/// Calcule la similarité cosinus entre deux vecteurs d'empreinte
pub fn cosine_similarity(v1: &[f32], v2: &[f32]) -> f32 {
    if v1.len() != v2.len() || v1.is_empty() {
        return 0.0;
    }

    let similarity: f32 = v1.iter().zip(v2.iter()).map(|(a, b)| a * b).sum();

    similarity.clamp(-1.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn person(name: &str, embedding: Vec<f32>) -> KnownPerson {
        KnownPerson {
            name: name.to_string(),
            embedding,
        }
    }

    // --- cosine_similarity ---

    #[test]
    fn cosine_similarity_of_identical_normalized_vectors_is_one() {
        let v = vec![1.0, 0.0, 0.0];
        assert!((cosine_similarity(&v, &v) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_of_orthogonal_vectors_is_zero() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        assert!((cosine_similarity(&a, &b)).abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_of_opposite_vectors_is_minus_one() {
        let a = vec![1.0, 0.0];
        let b = vec![-1.0, 0.0];
        assert!((cosine_similarity(&a, &b) + 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_of_mismatched_lengths_is_zero() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![1.0, 0.0];
        assert_eq!(cosine_similarity(&a, &b), 0.0);
    }

    #[test]
    fn cosine_similarity_of_empty_vectors_is_zero() {
        let empty: Vec<f32> = Vec::new();
        assert_eq!(cosine_similarity(&empty, &empty), 0.0);
    }

    #[test]
    fn cosine_similarity_result_is_always_clamped_to_valid_range() {
        // Vecteurs non normalisés : le produit scalaire brut pourrait
        // dépasser [-1.0, 1.0], ce que la fonction doit tout de même
        // clamper (elle suppose des vecteurs normalisés en usage normal).
        let a = vec![10.0, 10.0];
        let b = vec![10.0, 10.0];
        assert_eq!(cosine_similarity(&a, &b), 1.0);
    }

    // --- FaceEmbedder::identify_person ---

    #[test]
    fn identify_person_returns_none_when_no_known_people() {
        let embedding = vec![1.0, 0.0];
        let result = FaceEmbedder::identify_person(&embedding, &[], 0.5);
        assert_eq!(result, None);
    }

    #[test]
    fn identify_person_returns_none_when_best_match_is_below_threshold() {
        let embedding = vec![1.0, 0.0];
        let known = vec![person("jerome", vec![0.0, 1.0])]; // similarité 0.0
        let result = FaceEmbedder::identify_person(&embedding, &known, 0.5);
        assert_eq!(result, None);
    }

    #[test]
    fn identify_person_returns_the_matching_person_above_threshold() {
        let embedding = vec![1.0, 0.0];
        let known = vec![person("jerome", vec![1.0, 0.0])]; // similarité 1.0
        let result = FaceEmbedder::identify_person(&embedding, &known, 0.5);
        let (name, similarity) = result.expect("devrait matcher jerome");
        assert_eq!(name, "jerome");
        assert!((similarity - 1.0).abs() < 1e-6);
    }

    #[test]
    fn identify_person_picks_the_best_match_among_several_candidates() {
        let embedding = vec![1.0, 0.0];
        let known = vec![
            person("alice", vec![0.6, 0.8]), // similarité 0.6
            person("bob", vec![1.0, 0.0]),   // similarité 1.0 (meilleur)
            person("carol", vec![0.7, 0.7]), // similarité ~0.7
        ];
        let (name, _similarity) = FaceEmbedder::identify_person(&embedding, &known, 0.5)
            .expect("devrait matcher quelqu'un");
        assert_eq!(name, "bob");
    }

    #[test]
    fn identify_person_allows_multiple_templates_for_the_same_name() {
        // Plusieurs gabarits pour "jerome" (voir load_known_faces) : le
        // meilleur score parmi ses gabarits doit être retenu.
        let embedding = vec![1.0, 0.0];
        let known = vec![
            person("jerome", vec![0.1, 0.99]), // mauvais gabarit
            person("jerome", vec![1.0, 0.0]),  // bon gabarit
        ];
        let (name, similarity) =
            FaceEmbedder::identify_person(&embedding, &known, 0.5).expect("devrait matcher jerome");
        assert_eq!(name, "jerome");
        assert!((similarity - 1.0).abs() < 1e-6);
    }
}
