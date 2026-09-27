//! Bibliothèque `foxguard-manager` : regroupe les modules du manager pour
//! qu'ils soient utilisables à la fois par `src/main.rs` (le binaire, qui ne
//! fait qu'orchestrer le démarrage) et par de futurs tests d'intégration du
//! dossier `tests/` (qui ne peuvent dépendre que d'une bibliothèque, pas d'un
//! binaire).
//!
//! Même découpage binaire/bibliothèque que `foxguard-camera`, pour la même
//! raison.

pub mod api;
pub mod config;
pub mod ingest;
pub mod store;
