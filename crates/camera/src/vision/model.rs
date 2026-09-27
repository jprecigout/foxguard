//! Chargement mutualisé des modèles ONNX utilisés par les trois étapes du
//! pipeline de vision ([`super::object_detector`], [`super::face_detector`],
//! [`super::face_recognition`]).

use anyhow::Result;
use std::sync::Arc;
use tract_onnx::prelude::*;

/// Charge un modèle ONNX depuis `path` et le compile pour une entrée NCHW
/// `[1, 3, height, width]`. Factorise le chargement partagé par
/// `ObjectDetector::new`, `FaceDetectorYuNet::new` et `FaceEmbedder::new`,
/// qui ne diffèrent que par le chemin du modèle et la taille d'entrée.
pub(super) fn load_onnx_model(path: &str, width: u32, height: u32) -> Result<Arc<TypedSimplePlan>> {
    tract_onnx::onnx()
        .model_for_path(path)?
        .with_input_fact(
            0,
            InferenceFact::dt_shape(f32::datum_type(), [1, 3, height as i64, width as i64]),
        )?
        .into_optimized()?
        .into_runnable()
}
