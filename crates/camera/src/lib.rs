//! Bibliothèque FoxGuard : regroupe tous les modules de l'application pour
//! qu'ils soient utilisables à la fois par `src/main.rs` (le binaire, qui ne
//! fait plus que lancer [`run`] / [`print_banner`]) et par les tests
//! d'intégration du dossier `tests/` (qui ne peuvent dépendre que d'une
//! bibliothèque, pas d'un binaire).
//!
//! Le découpage en modules reste inchangé (voir la doc de chaque module) ;
//! seule la frontière binaire/bibliothèque a été introduite ici.

pub mod api;
pub mod capture;
pub mod config;
pub mod geometry;
pub mod h264;
pub mod mail;
pub mod mqtt;
pub mod retention;
pub mod rtsp;
pub mod util;
pub mod vision;
