//! Vision par ordinateur : détection d'objets (YOLO), détection de visage
//! (YuNet) et reconnaissance faciale (ArcFace). Chaque étape a son propre
//! module ; c'est `camera.rs` qui les enchaîne (personne -> visage -> identité).

mod face_detector;
mod face_recognition;
mod model;
mod object_detector;
mod types;

pub use face_detector::FaceDetectorYuNet;
pub use face_recognition::{FaceEmbedder, cosine_similarity};
pub use object_detector::ObjectDetector;
pub use types::{BoundingBox, KnownPerson};
