//! Bibliothèque de visages : le SEUL composant qui écrit dans
//! `known_faces/`, et celui qui partage ce dossier avec les autres caméras.
//!
//! # Trois sources de changements, un seul gardien
//!
//! - une CAPTURE demandée depuis une interface — celle de la caméra, ou celle
//!   du manager munie d'un ticket d'enrôlement (voir `crate::api`) ;
//! - une SUPPRESSION demandée de la même façon ;
//! - une photo, ou une suppression, PUBLIÉE par une autre caméra.
//!
//! Toutes passent par une seule tâche qui les traite l'une après l'autre :
//! deux écritures concurrentes du même dossier — une capture pendant qu'une
//! photo reçue est écrite, une suppression pendant un rechargement — n'ont
//! ainsi jamais à être arbitrées. Le travail lourd (analyse du visage,
//! disque, empreintes) part sur le pool bloquant de Tokio : ni la boucle de
//! capture ni le client MQTT n'attendent après lui.
//!
//! # Partage
//!
//! Actif quand MQTT l'est ET que `[server] stream_ticket_secret` est
//! renseigné : chaque photo est publiée signée, en message retenu, et toute
//! photo reçue est vérifiée avant d'être écrite (voir
//! `foxguard_protocol::faces` pour le format, et pour pourquoi une photo non
//! signée serait une faille). Sans secret, la caméra garde ses photos pour
//! elle, comme avant.
//!
//! # Réconciliation
//!
//! À chaque connexion au broker, les photos retenues sont relivrées. Une fois
//! le flot tari ([`SETTLE_DELAY`]), toute photo locale que le broker n'a pas
//! mentionnée — ni comme photo, ni comme supprimée — est publiée : celles
//! capturées pendant une coupure, et celles qui précèdent le partage
//! (déposées à la main, ou capturées avant cette version). Une photo
//! supprimée ailleurs pendant que la caméra était éteinte a, elle, laissé sa
//! pierre tombale : elle est effacée ici au lieu d'être republiée.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Local, Utc};
use foxguard_protocol::faces::{self, FaceMessage, FaceMessageError};
use image::{ImageFormat, RgbImage};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, sleep_until};
use tracing::{debug, error, info, warn};

use crate::mqtt::{FaceTopicEvent, MqttPublisher};
use crate::util::MutexExt;
use crate::vision::{FaceDetectorYuNet, FaceEmbedder, KnownPerson};

use super::known_faces::{EmbeddingCache, detect_face_native_res, list_photos, load_known_faces};

/// Réponse à une capture ou à une suppression : le fichier concerné, ou un
/// message destiné à la personne devant l'interface.
pub type FaceReply = Result<String, String>;

/// Ce qu'on demande à la bibliothèque.
pub enum FaceCommand {
    /// Ajouter cette frame comme photo de référence de `name`.
    Capture {
        name: String,
        image: RgbImage,
        reply: oneshot::Sender<FaceReply>,
    },
    /// Supprimer une photo de référence, ici et sur toutes les caméras.
    Delete {
        file: String,
        reply: oneshot::Sender<FaceReply>,
    },
}

/// Silence après la dernière photo relivrée par le broker au-delà duquel on
/// considère qu'il a tout remis, et qu'on peut publier ce qu'il ignore.
///
/// Les messages retenus arrivent d'un bloc juste après l'abonnement : sur un
/// réseau local, trois secondes de calme valent la fin du flot.
const SETTLE_DELAY: Duration = Duration::from_secs(3);

/// Délai de regroupement des rechargements de la base : une caméra qui
/// rejoint l'installation reçoit des dizaines de photos d'un coup, et doit
/// recharger UNE fois, pas une par photo.
const RELOAD_DELAY: Duration = Duration::from_millis(500);

/// Longueur maximale d'un nom de personne : le nom de fichier qui le porte
/// (`<nom>_<horodatage>.jpg`) doit rester partageable (voir
/// `foxguard_protocol::faces::is_valid_file_name`).
const MAX_NAME_LEN: usize = 64;

/// De quoi partager les photos (voir l'en-tête du module).
pub(super) struct Sharing {
    pub(super) mqtt: MqttPublisher,
    pub(super) secret: Vec<u8>,
}

/// La bibliothèque, avant son démarrage par [`FaceLibrary::spawn`].
pub(super) struct FaceLibrary {
    pub(super) dir: PathBuf,
    /// Nom de cette caméra, qui signe ce qu'elle publie.
    pub(super) camera: String,
    pub(super) face_detector: Arc<Option<FaceDetectorYuNet>>,
    pub(super) face_embedder: Arc<Option<FaceEmbedder>>,
    /// Base de reconnaissance, lue par le worker (voir `super::worker`).
    pub(super) known_people: Arc<Mutex<Vec<KnownPerson>>>,
    /// Empreintes déjà calculées au chargement initial.
    pub(super) cache: EmbeddingCache,
    pub(super) sharing: Option<Sharing>,
}

impl FaceLibrary {
    /// Démarre la tâche de la bibliothèque.
    ///
    /// `topic` : ce que le broker relaie du topic des photos, si le partage
    /// est actif.
    pub(super) fn spawn(
        self,
        commands: mpsc::UnboundedReceiver<FaceCommand>,
        topic: Option<mpsc::UnboundedReceiver<FaceTopicEvent>>,
    ) {
        tokio::spawn(self.run(commands, topic));
    }

    async fn run(
        mut self,
        mut commands: mpsc::UnboundedReceiver<FaceCommand>,
        mut topic: Option<mpsc::UnboundedReceiver<FaceTopicEvent>>,
    ) {
        // Fichiers dont le broker a parlé depuis la dernière connexion, comme
        // photo ou comme pierre tombale (voir la réconciliation, en tête de
        // module).
        let mut seen: HashSet<String> = HashSet::new();
        let mut reconcile_at: Option<Instant> = None;
        let mut reload_at: Option<Instant> = None;

        loop {
            tokio::select! {
                command = commands.recv() => {
                    let Some(command) = command else { break };

                    if self.handle_command(command, &mut seen).await {
                        reload_at.get_or_insert_with(|| Instant::now() + RELOAD_DELAY);
                    }
                }

                event = next_topic_event(&mut topic) => match event {
                    // Le client MQTT a disparu : plus rien à attendre de lui.
                    None => topic = None,

                    // Tout ce que le broker savait va être relivré : on
                    // repart de zéro, ce qui rattrape aussi un broker qui
                    // aurait perdu ses messages retenus.
                    Some(FaceTopicEvent::Connected) => {
                        seen.clear();
                        reconcile_at = Some(Instant::now() + SETTLE_DELAY);
                    }

                    Some(FaceTopicEvent::Message { file, payload }) => {
                        if reconcile_at.is_some() {
                            reconcile_at = Some(Instant::now() + SETTLE_DELAY);
                        }

                        if self.handle_received(file, payload, &mut seen).await {
                            reload_at.get_or_insert_with(|| Instant::now() + RELOAD_DELAY);
                        }
                    }
                },

                () = sleep_until(deadline(reconcile_at)), if reconcile_at.is_some() => {
                    reconcile_at = None;
                    self.reconcile(&mut seen).await;
                }

                () = sleep_until(deadline(reload_at)), if reload_at.is_some() => {
                    reload_at = None;
                    self.reload().await;
                }
            }
        }
    }

    /// Traite une capture ou une suppression. Vrai si le dossier a changé.
    async fn handle_command(&mut self, command: FaceCommand, seen: &mut HashSet<String>) -> bool {
        match command {
            FaceCommand::Capture { name, image, reply } => {
                let dir = self.dir.clone();
                let detector = Arc::clone(&self.face_detector);

                let result = tokio::task::spawn_blocking(move || {
                    capture(&dir, &name, &image, (*detector).as_ref())
                })
                .await
                .unwrap_or_else(|e| {
                    error!("❌ Capture de référence interrompue : {e}");
                    Err("La capture a échoué sur la caméra.".to_string())
                });

                match result {
                    Ok(saved) => {
                        info!(
                            "📸 Photo de référence capturée pour '{}' → {}",
                            faces::person_name(&saved.file),
                            self.dir.join(&saved.file).display()
                        );
                        self.share_photo(&saved.file, &saved.bytes, seen).await;
                        let _ = reply.send(Ok(saved.file));
                        true
                    }
                    Err(message) => {
                        warn!("⚠️ Photo de référence refusée : {message}");
                        let _ = reply.send(Err(message));
                        false
                    }
                }
            }

            FaceCommand::Delete { file, reply } => {
                let dir = self.dir.clone();
                let shared = self.sharing.is_some();
                let target = file.clone();

                let result = tokio::task::spawn_blocking(move || remove(&dir, &target, shared))
                    .await
                    .unwrap_or_else(|e| {
                        error!("❌ Suppression de référence interrompue : {e}");
                        Err("La suppression a échoué sur la caméra.".to_string())
                    });

                match result {
                    Ok(removed_here) => {
                        info!("🗑️ Photo de référence supprimée : {file}");

                        if let Some(sharing) = &self.sharing {
                            let removal = FaceMessage::removal(
                                &sharing.secret,
                                &file,
                                &self.camera,
                                Utc::now(),
                            );
                            self.publish(removal, seen).await;
                        }

                        let _ = reply.send(Ok(file));
                        removed_here
                    }
                    Err(message) => {
                        warn!("⚠️ Suppression de {file} refusée : {message}");
                        let _ = reply.send(Err(message));
                        false
                    }
                }
            }
        }
    }

    /// Traite un message reçu sur le topic d'une photo. Vrai si le dossier a
    /// changé.
    async fn handle_received(
        &mut self,
        file: String,
        payload: Vec<u8>,
        seen: &mut HashSet<String>,
    ) -> bool {
        let Some(sharing) = &self.sharing else {
            return false;
        };

        // Message retenu effacé à la main (`mosquitto_pub -r -n`) : le broker
        // ne sait plus rien de ce fichier. Ce n'est PAS une suppression —
        // elle passe par une pierre tombale — et la réconciliation le
        // republiera si la photo est ici.
        if payload.is_empty() {
            debug!("📭 Topic de {file} vidé sur le broker.");
            return false;
        }

        let message = match FaceMessage::parse(&file, &payload, &sharing.secret) {
            Ok(message) => message,
            Err(e) => {
                // Pas compté comme « vu » : si la photo légitime est ici, la
                // réconciliation la republiera par-dessus.
                warn!("⚠️ Photo partagée refusée ({file}) : {}", describe(&e));
                return false;
            }
        };

        seen.insert(file);

        let dir = self.dir.clone();
        tokio::task::spawn_blocking(move || apply(&dir, &message))
            .await
            .unwrap_or(false)
    }

    /// Publie une photo qui vient d'être capturée ici.
    async fn share_photo(&self, file: &str, bytes: &[u8], seen: &mut HashSet<String>) {
        let Some(sharing) = &self.sharing else {
            return;
        };

        if bytes.len() > faces::MAX_PHOTO_BYTES {
            warn!(
                "⚠️ Photo {file} trop volumineuse pour être partagée ({} octets) : elle reste sur cette caméra.",
                bytes.len()
            );
            return;
        }

        let message = FaceMessage::photo(&sharing.secret, file, &self.camera, Utc::now(), bytes);
        self.publish(message, seen).await;
    }

    async fn publish(&self, message: FaceMessage, seen: &mut HashSet<String>) {
        let Some(sharing) = &self.sharing else {
            return;
        };

        match sharing.mqtt.publish_face(&message).await {
            Ok(()) => {
                seen.insert(message.file().to_string());
            }
            // Pas « vu » : la réconciliation de la prochaine connexion
            // retentera.
            Err(e) => error!("❌ Partage de {} impossible : {e:#}", message.file()),
        }
    }

    /// Publie les photos locales que le broker ignore (voir l'en-tête du
    /// module).
    async fn reconcile(&self, seen: &mut HashSet<String>) {
        let Some(sharing) = &self.sharing else {
            return;
        };

        let dir = self.dir.clone();
        let known = seen.clone();
        let camera = self.camera.clone();
        let secret = sharing.secret.clone();

        let messages =
            tokio::task::spawn_blocking(move || unshared_photos(&dir, &known, &camera, &secret))
                .await
                .unwrap_or_default();

        if !messages.is_empty() {
            info!(
                "📤 Partage de {} photo(s) de référence encore inconnue(s) des autres caméras.",
                messages.len()
            );
        }

        for message in messages {
            self.publish(message, seen).await;
        }
    }

    /// Recharge la base de reconnaissance, en ne recalculant que les photos
    /// nouvelles (voir [`EmbeddingCache`]).
    async fn reload(&mut self) {
        if self.face_detector.is_none() || self.face_embedder.is_none() {
            return;
        }

        let detector = Arc::clone(&self.face_detector);
        let embedder = Arc::clone(&self.face_embedder);
        let dir = self.dir.clone();
        let mut cache = std::mem::take(&mut self.cache);

        let result = tokio::task::spawn_blocking(move || {
            let fresh = match (detector.as_ref(), embedder.as_ref()) {
                (Some(detector), Some(embedder)) => {
                    load_known_faces(detector, embedder, &dir, &mut cache)
                }
                _ => Vec::new(),
            };
            (cache, fresh)
        })
        .await;

        match result {
            Ok((cache, fresh)) => {
                self.cache = cache;
                *self.known_people.lock_or_recover() = fresh;
            }
            Err(e) => error!("❌ Rechargement de la base de reconnaissance interrompu : {e}"),
        }
    }
}

/// Prochain événement du broker, ou jamais s'il n'y a pas de partage.
async fn next_topic_event(
    topic: &mut Option<mpsc::UnboundedReceiver<FaceTopicEvent>>,
) -> Option<FaceTopicEvent> {
    match topic {
        Some(events) => events.recv().await,
        None => std::future::pending().await,
    }
}

/// Échéance d'une minuterie facultative. Une branche désactivée de `select!`
/// évalue quand même son expression : il lui faut une échéance, même si elle
/// n'est jamais attendue.
fn deadline(at: Option<Instant>) -> Instant {
    at.unwrap_or_else(|| Instant::now() + Duration::from_secs(24 * 3600))
}

/// Cause d'un refus, pour le journal.
fn describe(error: &FaceMessageError) -> &'static str {
    match error {
        FaceMessageError::Malformed => "message illisible",
        FaceMessageError::InvalidFileName => "nom de fichier refusé",
        FaceMessageError::TopicMismatch => "fichier différent de celui du topic",
        FaceMessageError::TooLarge => "photo trop volumineuse",
        FaceMessageError::BadSignature => {
            "signature invalide (secret des tickets différent sur l'émetteur, ou message falsifié)"
        }
    }
}

/// Une photo écrite dans `known_faces/`.
struct SavedPhoto {
    file: String,
    bytes: Vec<u8>,
}

/// Vérifie qu'un nom de personne peut nommer un fichier partageable, et le
/// rend débarrassé de ses espaces de bord.
///
/// REFUSÉ plutôt que nettoyé : « Jérôme » devenait silencieusement « Jrme »,
/// et la personne était ensuite reconnue sous un nom qu'elle n'avait pas
/// saisi. Mieux vaut le lui dire au moment de la capture.
fn validate_name(name: &str) -> Result<&str, String> {
    let name = name.trim();

    let valid = !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');

    if valid {
        Ok(name)
    } else {
        Err(format!(
            "Nom invalide : utilisez de 1 à {MAX_NAME_LEN} lettres sans accents, chiffres, « - » ou « _ »."
        ))
    }
}

/// Vérifie la frame, puis l'écrit comme nouvelle photo de référence de
/// `name`.
///
/// La vérification compte surtout à distance : depuis le manager, une photo
/// sans visage exploitable (personne trop loin, de profil, à contre-jour)
/// était enregistrée quand même, puis ignorée en silence au chargement — sur
/// toutes les caméras.
///
/// `detector` absent (modèle YuNet non chargé) : la photo est acceptée sans
/// vérification. La reconnaissance est de toute façon hors service sur cette
/// caméra, mais la photo servira aux autres.
fn capture(
    dir: &Path,
    name: &str,
    image: &RgbImage,
    detector: Option<&FaceDetectorYuNet>,
) -> Result<SavedPhoto, String> {
    let name = validate_name(name)?;

    if let Some(detector) = detector {
        match detect_face_native_res(detector, image) {
            Ok(Some(_)) => {}
            Ok(None) => {
                return Err(
                    "Aucun visage détecté : placez-vous face à la caméra, plus près, dans un endroit bien éclairé."
                        .to_string(),
                );
            }
            Err(e) => {
                error!("❌ Erreur YuNet sur la capture de référence : {e:?}");
                return Err("Analyse du visage impossible sur cette image.".to_string());
            }
        }
    }

    // Suffixe temporel : chaque capture s'ajoute comme un nouveau "gabarit"
    // pour cette personne au lieu d'écraser les précédentes (voir
    // `foxguard_protocol::faces::person_name_from_stem`). À la milliseconde :
    // deux caméras qui capturent la même personne au même instant ne
    // produisent pas le même nom de fichier — ou alors, à cette précision,
    // c'est le même instant et la dernière l'emporte sans dommage.
    let file = format!("{name}_{}.jpg", Local::now().format("%Y%m%d%H%M%S%3f"));

    let mut bytes = Vec::new();
    image
        .write_to(&mut std::io::Cursor::new(&mut bytes), ImageFormat::Jpeg)
        .map_err(|e| {
            error!("❌ Encodage de la photo de référence impossible : {e}");
            "Encodage de la photo impossible.".to_string()
        })?;

    write_atomically(&dir.join(&file), &bytes).map_err(|e| {
        error!("❌ Échec de sauvegarde de la photo de référence {file} : {e}");
        "Écriture de la photo impossible sur la caméra.".to_string()
    })?;

    Ok(SavedPhoto { file, bytes })
}

/// Supprime une photo de `known_faces/`. Vrai si elle était ici.
///
/// Absente d'ici mais partagée, ce n'est pas une erreur : elle vit peut-être
/// sur d'autres caméras, que la pierre tombale atteindra.
fn remove(dir: &Path, file: &str, shared: bool) -> Result<bool, String> {
    if !faces::is_valid_file_name(file) {
        return Err("Nom de photo invalide.".to_string());
    }

    match std::fs::remove_file(dir.join(file)) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && shared => Ok(false),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Err("Photo introuvable sur cette caméra.".to_string())
        }
        Err(e) => {
            error!("❌ Suppression de {file} impossible : {e}");
            Err("Suppression impossible sur la caméra.".to_string())
        }
    }
}

/// Applique au dossier un message reçu et VÉRIFIÉ. Vrai si le dossier a
/// changé.
fn apply(dir: &Path, message: &FaceMessage) -> bool {
    let path = dir.join(message.file());

    match message {
        FaceMessage::Photo(photo) => {
            // Décodable : `FaceMessage::parse` l'a vérifié.
            let Some(bytes) = photo.decoded_image() else {
                return false;
            };

            // Déjà là, à l'identique : notre propre publication qui revient,
            // ou une relivraison après reconnexion.
            if std::fs::read(&path).is_ok_and(|current| current == bytes) {
                return false;
            }

            match write_atomically(&path, &bytes) {
                Ok(()) => {
                    info!(
                        "📥 Photo de référence reçue de « {} » : {}",
                        photo.camera, photo.file
                    );
                    true
                }
                Err(e) => {
                    error!(
                        "❌ Écriture de la photo reçue {} impossible : {e}",
                        photo.file
                    );
                    false
                }
            }
        }

        FaceMessage::Removed(removal) => match std::fs::remove_file(&path) {
            Ok(()) => {
                info!(
                    "🗑️ Photo de référence supprimée depuis « {} » : {}",
                    removal.camera, removal.file
                );
                true
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => {
                error!("❌ Suppression de {} impossible : {e}", removal.file);
                false
            }
        },
    }
}

/// Photos locales dont le broker n'a pas parlé, signées et prêtes à publier.
fn unshared_photos(
    dir: &Path,
    seen: &HashSet<String>,
    camera: &str,
    secret: &[u8],
) -> Vec<FaceMessage> {
    list_photos(dir)
        .into_iter()
        .filter_map(|path| {
            let file = path.file_name()?.to_str()?;

            if seen.contains(file) {
                return None;
            }

            if !faces::is_valid_file_name(file) {
                warn!(
                    "⚠️ Photo de référence « {file} » non partagée : renommez-la avec des lettres \
                     sans accents, des chiffres, « - » ou « _ »."
                );
                return None;
            }

            let bytes = std::fs::read(&path).ok()?;

            if bytes.len() > faces::MAX_PHOTO_BYTES {
                warn!(
                    "⚠️ Photo de référence « {file} » trop volumineuse pour être partagée ({} octets).",
                    bytes.len()
                );
                return None;
            }

            let captured_at: DateTime<Utc> = std::fs::metadata(&path)
                .and_then(|metadata| metadata.modified())
                .map(DateTime::from)
                .unwrap_or_else(|_| Utc::now());

            Some(FaceMessage::photo(secret, file, camera, captured_at, &bytes))
        })
        .collect()
}

/// Écrit un fichier sous un nom temporaire, puis le renomme.
///
/// Un rechargement de la base qui tomberait sur une photo à moitié écrite la
/// jugerait illisible ; le renommage, lui, est atomique. Le nom temporaire
/// (`.<fichier>.tmp`) n'a pas d'extension d'image : il n'est jamais chargé.
fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;

    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default();
    let temporary = dir.join(format!(".{file_name}.tmp"));

    std::fs::write(&temporary, bytes)?;
    std::fs::rename(&temporary, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";

    fn photo_message(file: &str, bytes: &[u8]) -> FaceMessage {
        FaceMessage::photo(SECRET, file, "entree", Utc::now(), bytes)
    }

    // --- Capture ---

    #[test]
    fn a_capture_writes_a_timestamped_jpeg_named_after_the_person() {
        let dir = tempfile::tempdir().unwrap();

        let saved = capture(dir.path(), "  lou  ", &RgbImage::new(32, 24), None).expect("capture");

        assert!(faces::is_valid_file_name(&saved.file), "{}", saved.file);
        assert_eq!(faces::person_name(&saved.file), "lou");
        assert_eq!(
            std::fs::read(dir.path().join(&saved.file)).unwrap(),
            saved.bytes
        );
        assert_eq!(&saved.bytes[..2], &[0xFF, 0xD8], "un JPEG");
    }

    #[test]
    fn a_name_that_would_be_mangled_is_refused_rather_than_cleaned() {
        let dir = tempfile::tempdir().unwrap();

        for name in ["", "   ", "Jérôme", "jean paul", "../lou", &"a".repeat(65)] {
            assert!(
                capture(dir.path(), name, &RgbImage::new(8, 8), None).is_err(),
                "{name:?}"
            );
        }

        assert!(list_photos(dir.path()).is_empty());
    }

    // --- Suppression ---

    #[test]
    fn removing_a_local_photo_reports_that_it_was_here() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("lou_1.jpg"), b"x").unwrap();

        assert_eq!(remove(dir.path(), "lou_1.jpg", false), Ok(true));
        assert!(!dir.path().join("lou_1.jpg").exists());
    }

    #[test]
    fn a_shared_photo_absent_from_here_can_still_be_removed_everywhere() {
        let dir = tempfile::tempdir().unwrap();

        assert_eq!(remove(dir.path(), "lou_1.jpg", true), Ok(false));
        assert!(remove(dir.path(), "lou_1.jpg", false).is_err());
    }

    #[test]
    fn a_removal_cannot_leave_the_folder() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("dehors.jpg");
        std::fs::write(&outside, b"x").unwrap();
        let folder = dir.path().join("known_faces");

        assert!(remove(&folder, "../dehors.jpg", true).is_err());
        assert!(outside.exists());
    }

    // --- Messages reçus ---

    #[test]
    fn a_received_photo_is_written_once() {
        let dir = tempfile::tempdir().unwrap();
        let message = photo_message("lou_1.jpg", b"jpeg");

        assert!(apply(dir.path(), &message), "nouvelle photo");
        assert_eq!(
            std::fs::read(dir.path().join("lou_1.jpg")).unwrap(),
            b"jpeg"
        );

        // Relivrée après une reconnexion, ou notre propre publication qui
        // revient : rien à recharger.
        assert!(!apply(dir.path(), &message));
    }

    #[test]
    fn a_received_photo_replaces_a_different_local_version() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("lou.jpg"), b"ancienne").unwrap();

        assert!(apply(dir.path(), &photo_message("lou.jpg", b"partagee")));
        assert_eq!(
            std::fs::read(dir.path().join("lou.jpg")).unwrap(),
            b"partagee"
        );
    }

    #[test]
    fn a_received_removal_deletes_the_local_photo() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("lou_1.jpg"), b"x").unwrap();
        let removal = FaceMessage::removal(SECRET, "lou_1.jpg", "jardin", Utc::now());

        assert!(apply(dir.path(), &removal));
        assert!(!dir.path().join("lou_1.jpg").exists());
        assert!(!apply(dir.path(), &removal), "déjà absente");
    }

    // --- Réconciliation ---

    #[test]
    fn only_photos_unknown_to_the_broker_are_shared_and_they_are_signed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("lou_1.jpg"), b"connue").unwrap();
        std::fs::write(dir.path().join("lou_2.jpg"), b"inconnue").unwrap();
        std::fs::write(dir.path().join("Jérôme.jpg"), b"nom non partageable").unwrap();

        let seen = HashSet::from(["lou_1.jpg".to_string()]);
        let messages = unshared_photos(dir.path(), &seen, "entree", SECRET);

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].file(), "lou_2.jpg");
        assert_eq!(messages[0].verify(SECRET), Ok(()));
    }

    // --- Tâche complète, sans partage ---

    fn library(dir: &Path) -> FaceLibrary {
        FaceLibrary {
            dir: dir.to_path_buf(),
            camera: "entree".to_string(),
            face_detector: Arc::new(None),
            face_embedder: Arc::new(None),
            known_people: Arc::new(Mutex::new(Vec::new())),
            cache: EmbeddingCache::default(),
            sharing: None,
        }
    }

    #[tokio::test]
    async fn the_library_answers_captures_and_removals() {
        let dir = tempfile::tempdir().unwrap();
        let (commands, inbox) = mpsc::unbounded_channel();
        library(dir.path()).spawn(inbox, None);

        let (reply, answer) = oneshot::channel();
        commands
            .send(FaceCommand::Capture {
                name: "lou".to_string(),
                image: RgbImage::new(16, 16),
                reply,
            })
            .unwrap();
        let file = answer.await.unwrap().expect("capture acceptée");
        assert!(dir.path().join(&file).exists());

        let (reply, answer) = oneshot::channel();
        commands
            .send(FaceCommand::Delete {
                file: file.clone(),
                reply,
            })
            .unwrap();
        assert_eq!(answer.await.unwrap(), Ok(file.clone()));
        assert!(!dir.path().join(&file).exists());

        // Sans partage, supprimer ce qui n'est pas ici est une erreur : il
        // n'y a nulle part ailleurs où la suppression pourrait servir.
        let (reply, answer) = oneshot::channel();
        commands.send(FaceCommand::Delete { file, reply }).unwrap();
        assert!(answer.await.unwrap().is_err());
    }

    // --- Contre un VRAI broker ---
    //
    // Ce qu'on vérifie ici — l'abonnement à chaque connexion, les messages
    // retenus relivrés à une caméra qui arrive, la limite de paquet, la
    // réconciliation — n'a aucun sens contre une imitation. Ignoré sans
    // broker, comme les tests PostgreSQL du manager. Pour l'exécuter :
    //
    //   docker run -d --rm --name fg-mqtt -p 51883:1883 eclipse-mosquitto:2 \
    //       mosquitto -c /mosquitto-no-auth.conf
    //   FOXGUARD_TEST_MQTT_HOST=localhost FOXGUARD_TEST_MQTT_PORT=51883 \
    //       cargo test -p foxguard-camera face_library
    //
    // Chaque exécution publie sous un préfixe de topic qui lui est propre, et
    // y laisse ses messages retenus : à réserver à un broker jetable.
    mod broker {
        use super::*;
        use crate::config::MqttConfig;
        use crate::mqtt::FaceTopic;

        fn broker() -> Option<(String, u16)> {
            let host = std::env::var("FOXGUARD_TEST_MQTT_HOST")
                .ok()
                .filter(|host| !host.is_empty())?;
            let port = std::env::var("FOXGUARD_TEST_MQTT_PORT")
                .ok()
                .and_then(|port| port.parse().ok())
                .unwrap_or(1883);
            Some((host, port))
        }

        fn config(host: &str, port: u16) -> MqttConfig {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let prefix = format!("foxguard-test-{}-{nanos}", std::process::id());

            MqttConfig {
                enabled: true,
                broker_host: host.to_string(),
                broker_port: port,
                username: String::new(),
                password: String::new(),
                topic: format!("{prefix}/detections"),
                faces_topic: format!("{prefix}/faces"),
            }
        }

        /// Une caméra réduite à sa bibliothèque de visages, partage actif.
        fn camera(
            name: &str,
            dir: &Path,
            config: &MqttConfig,
        ) -> mpsc::UnboundedSender<FaceCommand> {
            let (events, received) = mpsc::unbounded_channel();
            let mqtt = MqttPublisher::connect(config, name, Some(FaceTopic { events }));
            let (commands, inbox) = mpsc::unbounded_channel();

            FaceLibrary {
                camera: name.to_string(),
                sharing: Some(Sharing {
                    mqtt,
                    secret: SECRET.to_vec(),
                }),
                ..library(dir)
            }
            .spawn(inbox, Some(received));

            commands
        }

        async fn eventually(condition: impl Fn() -> bool) -> bool {
            for _ in 0..100 {
                if condition() {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            false
        }

        #[tokio::test]
        async fn photos_and_removals_reach_every_camera() {
            let Some((host, port)) = broker() else {
                eprintln!("FOXGUARD_TEST_MQTT_HOST absente : test ignoré");
                return;
            };
            let config = config(&host, port);

            let entree_dir = tempfile::tempdir().unwrap();
            let jardin_dir = tempfile::tempdir().unwrap();
            let (entree_path, jardin_path) = (
                entree_dir.path().to_path_buf(),
                jardin_dir.path().to_path_buf(),
            );

            // Déposée à la main avant le partage : elle doit être partagée à
            // la connexion (réconciliation).
            std::fs::write(jardin_path.join("mael.jpg"), b"photo deposee a la main").unwrap();

            let entree = camera("entree", &entree_path, &config);
            let jardin = camera("jardin", &jardin_path, &config);

            // Une capture devant l'entrée atteint le jardin, à l'identique.
            // Une vraie frame : la photo dépasse largement la limite de
            // paquet par défaut du client MQTT (10 Kio).
            let mut frame = RgbImage::new(640, 480);
            for (x, y, pixel) in frame.enumerate_pixels_mut() {
                *pixel = image::Rgb([(x % 256) as u8, (y % 256) as u8, ((x * y) % 256) as u8]);
            }
            let (reply, answer) = oneshot::channel();
            entree
                .send(FaceCommand::Capture {
                    name: "lou".to_string(),
                    image: frame,
                    reply,
                })
                .unwrap();
            let file = answer.await.unwrap().expect("capture acceptée");
            let captured = std::fs::read(entree_path.join(&file)).unwrap();
            assert!(captured.len() > 10 * 1024, "{} octets", captured.len());

            assert!(
                eventually(|| std::fs::read(jardin_path.join(&file)).is_ok_and(|b| b == captured))
                    .await,
                "la photo atteint l'autre caméra"
            );

            // La photo déposée à la main au jardin atteint l'entrée.
            assert!(
                eventually(|| entree_path.join("mael.jpg").exists()).await,
                "une photo antérieure au partage est partagée"
            );

            // Une suppression au jardin retire la photo de l'entrée.
            let (reply, answer) = oneshot::channel();
            jardin
                .send(FaceCommand::Delete {
                    file: file.clone(),
                    reply,
                })
                .unwrap();
            assert_eq!(answer.await.unwrap(), Ok(file.clone()));
            assert!(
                eventually(|| !entree_path.join(&file).exists()).await,
                "la suppression atteint l'autre caméra"
            );

            // Une caméra qui rejoint l'installation reçoit les photos en
            // cours — et pas celle qui a été supprimée entre-temps, même si
            // elle l'avait encore en local (caméra éteinte au moment de la
            // suppression).
            let garage_dir = tempfile::tempdir().unwrap();
            let garage_path = garage_dir.path().to_path_buf();
            std::fs::write(garage_path.join(&file), &captured).unwrap();
            let _garage = camera("garage", &garage_path, &config);

            assert!(
                eventually(|| garage_path.join("mael.jpg").exists()).await,
                "les photos retenues sont relivrées"
            );
            assert!(
                eventually(|| !garage_path.join(&file).exists()).await,
                "la pierre tombale efface la copie locale"
            );

            // La réconciliation du garage ne doit pas avoir ressuscité la
            // photo ailleurs.
            tokio::time::sleep(SETTLE_DELAY + Duration::from_secs(1)).await;
            assert!(!entree_path.join(&file).exists());
            assert!(!jardin_path.join(&file).exists());
            assert!(!garage_path.join(&file).exists());
        }

        #[tokio::test]
        async fn a_photo_signed_with_another_secret_is_ignored() {
            let Some((host, port)) = broker() else {
                eprintln!("FOXGUARD_TEST_MQTT_HOST absente : test ignoré");
                return;
            };
            let config = config(&host, port);

            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().to_path_buf();
            let _entree = camera("entree", &path, &config);

            // Quiconque atteint le broker peut publier ; seule la signature
            // fait la différence.
            let intruder = MqttPublisher::connect(&config, "intrus", None);
            let forged = FaceMessage::photo(
                b"un tout autre secret, assez long",
                "intrus_1.jpg",
                "intrus",
                Utc::now(),
                b"mon visage",
            );
            tokio::time::sleep(Duration::from_millis(500)).await;
            intruder.publish_face(&forged).await.expect("publication");

            tokio::time::sleep(Duration::from_secs(2)).await;
            assert!(!path.join("intrus_1.jpg").exists());
        }
    }
}
