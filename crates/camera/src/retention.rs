//! Suppression automatique des enregistrements trop anciens.
//!
//! Un système de vidéosurveillance qui enregistre en continu remplit son
//! disque en quelques jours : sans purge, le Raspberry Pi finit par ne plus
//! pouvoir écrire, et l'enregistrement s'arrête silencieusement. Une tâche de
//! fond ([`spawn_cleanup_task`]) balaie donc périodiquement le dossier des
//! enregistrements et supprime ceux qui dépassent la durée de conservation
//! configurée (`[recording] retention_days`, voir
//! [`crate::config::RecordingConfig`]).
//!
//! L'âge d'un fichier est déterminé par sa **date de dernière modification**,
//! et non par l'horodatage contenu dans son nom (`rec_20260918_120854.mp4`).
//! C'est un choix de sûreté : un enregistrement en cours d'écriture voit sa
//! date de modification rafraîchie en permanence, il ne peut donc jamais être
//! sélectionné pour suppression. Se fier au nom de fichier reviendrait à
//! effacer, en pleine écriture, un enregistrement démarré il y a plus
//! longtemps que la durée de rétention.

use std::fs;
use std::io;
use std::path::Path;
use std::time::{Duration, SystemTime};

use crate::config::RecordingConfig;
use tracing::{error, info, warn};

/// Extensions considérées comme des enregistrements. Tout autre fichier
/// présent dans le dossier est ignoré : la purge ne doit jamais toucher à
/// quelque chose qu'elle n'a pas écrit elle-même.
///
/// `mjpeg` n'est PLUS produit (voir [`crate::capture::RecordingFormat`]) mais
/// reste reconnu, et il doit le rester : une caméra mise à jour a des
/// fichiers de l'ancien format sur son disque. Les retirer de cette liste ne
/// les rendrait pas lisibles pour autant — ça les rendrait ÉTERNELS, et un
/// disque qui se remplit sans jamais se vider est précisément ce que ce
/// module existe pour éviter.
const RECORDING_EXTENSIONS: [&str; 2] = ["mjpeg", "mp4"];

/// Bilan d'un passage de nettoyage.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CleanupReport {
    /// Nombre de fichiers effectivement supprimés.
    pub deleted: usize,
    /// Espace disque libéré, en octets.
    pub freed_bytes: u64,
    /// Fichiers périmés dont la suppression a échoué (droits insuffisants,
    /// fichier verrouillé, ...). Ils seront retentés au passage suivant.
    pub failed: usize,
}

/// Démarre la tâche de fond de nettoyage des enregistrements.
///
/// Un premier passage a lieu immédiatement (voir la doc de
/// [`crate::config::RecordingConfig::cleanup_interval_secs`]), puis à chaque
/// intervalle configuré. Si `retention_days` vaut `0`, aucune tâche n'est
/// démarrée du tout.
pub fn spawn_cleanup_task(config: RecordingConfig) {
    if config.retention_days == 0 {
        info!("🗂️ Nettoyage automatique des enregistrements désactivé (retention_days = 0).");
        return;
    }

    let max_age = Duration::from_secs(config.retention_days * 24 * 60 * 60);
    let interval = Duration::from_secs(config.cleanup_interval_secs.max(1));
    let dir = config.dir.clone();

    info!(
        "🗂️ Nettoyage automatique des enregistrements : conservation {} jour(s), passage toutes les {} s.",
        config.retention_days,
        interval.as_secs()
    );

    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);

        loop {
            // Le premier `tick()` d'un `interval` est immédiat : le passage
            // de nettoyage a donc bien lieu au démarrage.
            ticker.tick().await;

            // `delete_expired` fait des appels systèmes bloquants (lecture de
            // dossier, suppressions) : on les confie au pool dédié plutôt que
            // de bloquer un thread de l'exécuteur asynchrone.
            let dir = dir.clone();
            let result =
                tokio::task::spawn_blocking(move || delete_expired(Path::new(&dir), max_age)).await;

            match result {
                Ok(Ok(report)) if report.deleted > 0 || report.failed > 0 => {
                    info!(
                        "🗂️ Nettoyage : {} enregistrement(s) supprimé(s) ({:.1} Mo libérés), {} échec(s).",
                        report.deleted,
                        report.freed_bytes as f64 / (1024.0 * 1024.0),
                        report.failed
                    );
                }
                // Rien à supprimer : on ne journalise pas, pour ne pas
                // remplir les logs d'un message horaire sans information.
                Ok(Ok(_)) => {}
                Ok(Err(e)) => {
                    error!(
                        "❌ Nettoyage des enregistrements impossible dans '{}' : {}",
                        config.dir, e
                    );
                }
                Err(e) => {
                    error!("❌ Tâche de nettoyage interrompue : {}", e);
                }
            }
        }
    });
}

/// Supprime les enregistrements de `dir` dont la dernière modification
/// remonte à plus de `max_age`, et retourne le bilan de l'opération.
///
/// Les erreurs portant sur un fichier PARTICULIER (métadonnées illisibles,
/// suppression refusée) sont comptabilisées dans [`CleanupReport::failed`] et
/// n'interrompent pas le balayage : un seul fichier problématique ne doit pas
/// empêcher la purge de tous les autres. Seule l'impossibilité de lire le
/// dossier lui-même remonte en `Err`.
///
/// Extrait de [`spawn_cleanup_task`] pour être testable sans tâche
/// asynchrone ni attente réelle (voir les tests en fin de fichier).
fn delete_expired(dir: &Path, max_age: Duration) -> io::Result<CleanupReport> {
    let mut report = CleanupReport::default();

    // Un dossier absent n'est pas une erreur : il sera créé au premier
    // enregistrement (voir `crate::api::create_router`).
    if !dir.exists() {
        return Ok(report);
    }

    let now = SystemTime::now();

    for entry in fs::read_dir(dir)? {
        let Ok(entry) = entry else {
            report.failed += 1;
            continue;
        };

        let path = entry.path();

        if !is_recording_file(&path) {
            continue;
        }

        let Ok(metadata) = entry.metadata() else {
            report.failed += 1;
            continue;
        };

        if !metadata.is_file() {
            continue;
        }

        let Ok(modified) = metadata.modified() else {
            report.failed += 1;
            continue;
        };

        // `duration_since` échoue si le fichier est daté dans le FUTUR
        // (horloge système reculée, fichier copié d'une autre machine) : on
        // le considère alors comme récent et on n'y touche pas.
        let Ok(age) = now.duration_since(modified) else {
            continue;
        };

        if age <= max_age {
            continue;
        }

        let size = metadata.len();

        match fs::remove_file(&path) {
            Ok(()) => {
                info!(
                    "🗑️ Enregistrement expiré supprimé : {} ({} jour(s))",
                    path.display(),
                    age.as_secs() / (24 * 60 * 60)
                );
                report.deleted += 1;
                report.freed_bytes += size;
            }
            Err(e) => {
                warn!("⚠️ Suppression impossible pour {} : {}", path.display(), e);
                report.failed += 1;
            }
        }
    }

    Ok(report)
}

/// Vrai si le chemin porte l'extension d'un enregistrement vidéo (voir
/// [`RECORDING_EXTENSIONS`]), insensible à la casse.
///
/// Partagé avec `crate::api` pour que la liste, le téléchargement, la
/// suppression manuelle et la purge automatique s'accordent toutes sur la
/// même définition de « ce qui est un enregistrement ».
pub fn is_recording_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| RECORDING_EXTENSIONS.contains(&ext.to_lowercase().as_str()))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;

    /// Crée un fichier dans `dir` et antidate sa dernière modification de
    /// `age`, ce qui permet de tester la purge sans attendre réellement.
    fn file_aged(dir: &Path, name: &str, age: Duration) -> std::path::PathBuf {
        let path = dir.join(name);
        let file = File::create(&path).expect("création du fichier de test");
        file.write_all_and_backdate(age);
        path
    }

    /// Petite extension locale pour antidater un fichier fraîchement créé.
    trait Backdate {
        fn write_all_and_backdate(&self, age: Duration);
    }

    impl Backdate for File {
        fn write_all_and_backdate(&self, age: Duration) {
            let modified = SystemTime::now() - age;
            let times = fs::FileTimes::new().set_modified(modified);
            self.set_times(times)
                .expect("antidatage du fichier de test");
        }
    }

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("dossier temporaire")
    }

    const ONE_DAY: Duration = Duration::from_secs(24 * 60 * 60);

    // --- is_recording_file ---

    #[test]
    fn both_the_current_and_the_former_extension_are_recognized() {
        assert!(is_recording_file(Path::new("rec_20260918_120854.mjpeg")));
        assert!(is_recording_file(Path::new("clip.mp4")));
    }

    #[test]
    fn extension_matching_is_case_insensitive() {
        assert!(is_recording_file(Path::new("REC.MJPEG")));
    }

    #[test]
    fn other_files_are_not_recordings() {
        assert!(!is_recording_file(Path::new("notes.txt")));
        assert!(!is_recording_file(Path::new("camera-config.toml")));
        // Sans extension du tout.
        assert!(!is_recording_file(Path::new("rec_20260918_120854")));
    }

    // --- delete_expired ---

    #[test]
    fn a_missing_directory_is_not_an_error() {
        let dir = tempdir();
        let absent = dir.path().join("inexistant");
        let report = delete_expired(&absent, ONE_DAY).expect("dossier absent toléré");
        assert_eq!(report, CleanupReport::default());
    }

    #[test]
    fn an_empty_directory_deletes_nothing() {
        let dir = tempdir();
        let report = delete_expired(dir.path(), ONE_DAY).expect("balayage");
        assert_eq!(report.deleted, 0);
    }

    #[test]
    fn a_recent_recording_is_kept() {
        let dir = tempdir();
        let path = file_aged(dir.path(), "rec_recent.mjpeg", Duration::from_secs(60));

        let report = delete_expired(dir.path(), 7 * ONE_DAY).expect("balayage");

        assert_eq!(report.deleted, 0);
        assert!(path.exists(), "un enregistrement récent doit être conservé");
    }

    #[test]
    fn an_expired_recording_is_deleted() {
        let dir = tempdir();
        let path = file_aged(dir.path(), "rec_vieux.mjpeg", 8 * ONE_DAY);

        let report = delete_expired(dir.path(), 7 * ONE_DAY).expect("balayage");

        assert_eq!(report.deleted, 1);
        assert!(
            !path.exists(),
            "un enregistrement expiré doit être supprimé"
        );
    }

    #[test]
    fn a_recording_exactly_at_the_limit_is_kept() {
        // La comparaison est `age > max_age` : à l'âge exact de la limite, le
        // fichier est conservé. Test aux bornes, pour que ce choix reste
        // explicite plutôt qu'accidentel.
        let dir = tempdir();
        let path = file_aged(dir.path(), "rec_limite.mjpeg", 7 * ONE_DAY);

        // On compare à une limite très légèrement supérieure à l'âge du
        // fichier, pour absorber le temps écoulé entre sa création et le
        // balayage.
        let report =
            delete_expired(dir.path(), 7 * ONE_DAY + Duration::from_secs(60)).expect("balayage");

        assert_eq!(report.deleted, 0);
        assert!(path.exists());
    }

    #[test]
    fn only_expired_recordings_are_deleted_among_several() {
        let dir = tempdir();
        let old_one = file_aged(dir.path(), "rec_vieux.mjpeg", 10 * ONE_DAY);
        let old_two = file_aged(dir.path(), "rec_ancien.mp4", 30 * ONE_DAY);
        let recent = file_aged(dir.path(), "rec_hier.mjpeg", ONE_DAY);

        let report = delete_expired(dir.path(), 7 * ONE_DAY).expect("balayage");

        assert_eq!(report.deleted, 2);
        assert!(!old_one.exists());
        assert!(!old_two.exists());
        assert!(recent.exists());
    }

    #[test]
    fn non_recording_files_are_never_touched_even_when_old() {
        // Garde-fou : la purge ne doit jamais effacer un fichier qu'elle n'a
        // pas écrit, quel que soit son âge.
        let dir = tempdir();
        let note = file_aged(dir.path(), "important.txt", 365 * ONE_DAY);
        let config = file_aged(dir.path(), "camera-config.toml", 365 * ONE_DAY);

        let report = delete_expired(dir.path(), 7 * ONE_DAY).expect("balayage");

        assert_eq!(report.deleted, 0);
        assert!(note.exists(), "un fichier non-vidéo doit être épargné");
        assert!(config.exists());
    }

    #[test]
    fn a_subdirectory_is_never_deleted() {
        let dir = tempdir();
        // Un sous-dossier nommé comme un enregistrement : il ne doit pas être
        // supprimé (`remove_file` échouerait de toute façon, mais on ne doit
        // même pas le tenter).
        let sub = dir.path().join("archives.mjpeg");
        fs::create_dir(&sub).expect("création du sous-dossier");

        let report = delete_expired(dir.path(), Duration::from_secs(0)).expect("balayage");

        assert_eq!(report.deleted, 0);
        assert_eq!(report.failed, 0);
        assert!(sub.exists());
    }

    #[test]
    fn the_report_accounts_for_the_freed_space() {
        let dir = tempdir();
        let path = file_aged(dir.path(), "rec_vieux.mjpeg", 8 * ONE_DAY);
        fs::write(&path, vec![0u8; 2048]).expect("écriture du contenu");
        // Réécrire le contenu a rafraîchi la date : on antidate de nouveau.
        let file = File::options()
            .write(true)
            .open(&path)
            .expect("ouverture du fichier de test");
        file.write_all_and_backdate(8 * ONE_DAY);

        let report = delete_expired(dir.path(), 7 * ONE_DAY).expect("balayage");

        assert_eq!(report.deleted, 1);
        assert_eq!(report.freed_bytes, 2048);
    }

    #[test]
    fn a_file_dated_in_the_future_is_kept() {
        // Horloge système reculée, ou fichier copié depuis une autre machine :
        // `duration_since` échoue. On conserve plutôt que de supprimer sur la
        // foi d'une date incohérente.
        let dir = tempdir();
        let path = dir.path().join("rec_futur.mjpeg");
        let file = File::create(&path).expect("création");
        let times = fs::FileTimes::new().set_modified(SystemTime::now() + 10 * ONE_DAY);
        file.set_times(times).expect("datation dans le futur");

        let report = delete_expired(dir.path(), Duration::from_secs(0)).expect("balayage");

        assert_eq!(report.deleted, 0);
        assert!(path.exists());
    }
}
