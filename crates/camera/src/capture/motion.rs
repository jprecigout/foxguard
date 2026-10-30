//! Pré-filtre de mouvement : décide si l'inférence YOLO vaut la peine d'être
//! lancée sur une frame.
//!
//! # Le problème
//!
//! Une caméra de surveillance regarde, l'immense majorité du temps, une scène
//! où il ne se passe rien. Or l'inférence YOLO est de loin le poste de
//! dépense dominant du pipeline sur un Raspberry Pi : la faire tourner sur
//! chacune de ces images identiques, c'est payer en permanence le prix fort
//! pour réapprendre à chaque passage que rien n'a changé.
//!
//! Comparer deux versions MINIATURES de l'image coûte, lui, quelques
//! microsecondes — quatre ordres de grandeur de moins. C'est donc ce filtre
//! qui est placé devant YOLO.
//!
//! # Comment
//!
//! Chaque frame candidate est réduite à une grille de
//! [`GRID_WIDTH`]×[`GRID_HEIGHT`] valeurs de luminance, chacune MOYENNÉE sur
//! la zone qu'elle représente. On compte ensuite les cases dont la luminance
//! s'écarte de plus de `pixel_threshold` du FOND de référence ; au-delà
//! d'une proportion `min_changed_ratio`, on considère qu'il y a mouvement
//! (voir [`crate::config::MotionConfig`]).
//!
//! # Le fond glissant
//!
//! Le fond n'est pas la frame précédente mais une MOYENNE GLISSANTE
//! exponentielle des frames candidates : à chaque évaluation, chaque case du
//! fond se rapproche de l'image courante d'une fraction
//! `1 - exp(-Δt / background_time_constant_secs)`. Pondérer par le temps
//! écoulé, et non par le nombre de frames, rend le fond indépendant de la
//! cadence d'évaluation, qui varie selon que YOLO tourne ou non.
//!
//! Comparer à la seule frame précédente avait deux défauts :
//!
//! - un mouvement LENT (quelqu'un qui avance doucement, une silhouette qui
//!   émerge de la pénombre) change peu d'une frame à l'autre et passait sous
//!   le seuil, alors qu'il s'écarte nettement d'un fond plus ancien ;
//! - une personne qui s'arrête cessait instantanément d'être « en
//!   mouvement ». Contre le fond, elle le reste jusqu'à y être absorbée.
//!
//! Le fond suit malgré tout les changements durables (lumière du jour, objet
//! déposé), sans quoi le filtre resterait ouvert pour de bon.
//!
//! Le moyennage n'est pas un détail : un simple échantillonnage ponctuel
//! ferait du bruit de capteur (très présent en basse lumière, précisément
//! quand une caméra de surveillance sert) un déclencheur permanent, et le
//! filtre ne filtrerait plus rien.
//!
//! # Les deux garde-fous
//!
//! Un filtre de mouvement naïf rate deux situations, et toutes deux comptent
//! pour de la surveillance :
//!
//! 1. **quelqu'un s'arrête.** Il ne produit plus de mouvement mais il est
//!    toujours là. Le filtre garde donc la porte ouverte pendant
//!    `hold_secs` après le dernier mouvement constaté ;
//! 2. **quelqu'un est déjà immobile.** Une présence absorbée par le fond (ou
//!    déjà là quand il s'est formé) ne s'en écarte plus. YOLO tourne donc de
//!    toute façon au moins une fois toutes les `max_idle_secs`.
//!
//! Sans ces deux règles, l'économie de CPU se paierait en détections
//! manquées — ce qui n'est pas un compromis acceptable pour un système de
//! surveillance.

use std::time::{Duration, Instant};

use image::RgbImage;

use crate::config::MotionConfig;

/// Largeur de la grille d'analyse.
///
/// 64×48 : assez fin pour qu'une silhouette au fond du champ occupe
/// plusieurs cases, assez grossier pour que la comparaison tienne dans
/// quelques milliers d'opérations.
pub const GRID_WIDTH: usize = 64;
/// Hauteur de la grille d'analyse.
pub const GRID_HEIGHT: usize = 48;

/// Nombre de points échantillonnés par case, sur chaque axe (soit 4 points
/// par case).
///
/// Moyenner sur plusieurs points est ce qui rend le filtre insensible au
/// bruit du capteur ; en prendre davantage ne change plus le verdict et
/// coûte du temps.
const SAMPLES_PER_CELL: u32 = 2;

/// Raison pour laquelle l'inférence est lancée — journalisée en `debug!`
/// pour permettre de régler les seuils de `[motion]` sans tâtonner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanReason {
    /// L'image a changé.
    Motion,
    /// Rien ne bouge, mais du mouvement a été constaté récemment : la
    /// personne s'est peut-être simplement arrêtée.
    AfterMotion,
    /// Passage périodique de sécurité : une présence parfaitement immobile
    /// ne produit aucun mouvement.
    Periodic,
    /// Première frame, ou filtre désactivé : il n'y a rien à comparer.
    NoReference,
}

/// Verdict du pré-filtre pour une frame.
#[derive(Debug, Clone, Copy)]
pub struct Verdict {
    /// Faut-il lancer YOLO sur cette frame ?
    pub scan: bool,
    /// Pourquoi, le cas échéant.
    pub reason: ScanReason,
    /// Proportion de cases ayant changé, de 0 à 1. Utile pour régler
    /// `min_changed_ratio` d'après les journaux plutôt qu'au hasard.
    pub changed_ratio: f32,
}

/// Fond de référence : moyenne glissante des miniatures des frames
/// CANDIDATES.
///
/// Candidates, et non toutes les frames capturées : le filtre n'est consulté
/// qu'aux frames où YOLO pourrait tourner (voir `super::worker`).
struct Background {
    /// Luminance moyenne de chaque case. En flottants : avec des entiers, les
    /// petits pas de la moyenne seraient arrondis à zéro et le fond ne
    /// suivrait plus une dérive lente.
    luma: Vec<f32>,
    /// Instant de la dernière mise à jour, d'où se déduit le poids de la
    /// suivante.
    updated: Instant,
}

impl Background {
    fn new(thumbnail: &[u8], now: Instant) -> Self {
        Self {
            luma: thumbnail.iter().map(|&value| f32::from(value)).collect(),
            updated: now,
        }
    }

    /// Rapproche le fond de `thumbnail`, d'autant plus que la dernière mise à
    /// jour est ancienne.
    fn update(&mut self, thumbnail: &[u8], now: Instant, time_constant_secs: f32) {
        let elapsed = now.saturating_duration_since(self.updated).as_secs_f32();
        self.updated = now;

        // Une constante nulle ramène à « le fond est la frame précédente ».
        let weight = if time_constant_secs > 0.0 {
            1.0 - (-elapsed / time_constant_secs).exp()
        } else {
            1.0
        };

        if self.luma.len() != thumbnail.len() {
            *self = Self::new(thumbnail, now);
            return;
        }

        for (background, &current) in self.luma.iter_mut().zip(thumbnail) {
            *background += weight * (f32::from(current) - *background);
        }
    }
}

/// Pré-filtre de mouvement, à raison d'une instance par flux (il porte le
/// fond de référence).
pub(super) struct MotionGate {
    config: MotionConfig,
    background: Option<Background>,
    last_motion: Option<Instant>,
    last_scan: Option<Instant>,
}

impl MotionGate {
    pub(super) fn new(config: MotionConfig) -> Self {
        Self {
            config,
            background: None,
            last_motion: None,
            last_scan: None,
        }
    }

    /// Décide si YOLO doit tourner sur `image`.
    ///
    /// À appeler sur les frames candidates uniquement, et une seule fois par
    /// frame : la méthode met à jour l'état interne du filtre.
    pub(super) fn evaluate(&mut self, image: &RgbImage) -> Verdict {
        self.evaluate_at(image, Instant::now())
    }

    /// Comme [`Self::evaluate`], mais à un instant fourni : c'est ce qui
    /// rend les garde-fous temporels testables sans faire dormir le test.
    fn evaluate_at(&mut self, image: &RgbImage, now: Instant) -> Verdict {
        if !self.config.enabled {
            self.last_scan = Some(now);

            return Verdict {
                scan: true,
                reason: ScanReason::NoReference,
                changed_ratio: 0.0,
            };
        }

        let thumbnail = downscale_luma(image);

        let had_reference = self.background.is_some();

        // La comparaison se fait AVANT la mise à jour du fond : sinon l'image
        // courante y serait déjà en partie, et masquerait son propre écart.
        let changed_ratio = match &mut self.background {
            Some(background) => {
                let ratio =
                    changed_ratio(&background.luma, &thumbnail, self.config.pixel_threshold);
                background.update(&thumbnail, now, self.config.background_time_constant_secs);
                ratio
            }
            // Première frame : aucun fond, donc aucun verdict possible. On
            // lance l'inférence — ne pas le faire reviendrait à ignorer la
            // scène déjà en place au démarrage.
            None => {
                self.background = Some(Background::new(&thumbnail, now));
                1.0
            }
        };

        let moving = had_reference && changed_ratio >= self.config.min_changed_ratio;

        if moving {
            self.last_motion = Some(now);
        }

        let reason = if !had_reference {
            Some(ScanReason::NoReference)
        } else if moving {
            Some(ScanReason::Motion)
        } else if self.within_motion_hold(now) {
            Some(ScanReason::AfterMotion)
        } else if self.periodic_scan_due(now) {
            Some(ScanReason::Periodic)
        } else {
            None
        };

        match reason {
            Some(reason) => {
                self.last_scan = Some(now);

                Verdict {
                    scan: true,
                    reason,
                    changed_ratio,
                }
            }
            None => Verdict {
                scan: false,
                reason: ScanReason::Motion,
                changed_ratio,
            },
        }
    }

    /// Premier garde-fou : du mouvement a été vu il y a moins de
    /// `hold_secs`.
    fn within_motion_hold(&self, now: Instant) -> bool {
        let hold = Duration::from_secs(self.config.hold_secs);

        self.last_motion
            .is_some_and(|last| now.duration_since(last) < hold)
    }

    /// Second garde-fou : `max_idle_secs` se sont écoulés depuis la dernière
    /// inférence. `0` le désactive.
    fn periodic_scan_due(&self, now: Instant) -> bool {
        if self.config.max_idle_secs == 0 {
            return false;
        }

        let max_idle = Duration::from_secs(self.config.max_idle_secs);

        match self.last_scan {
            Some(last) => now.duration_since(last) >= max_idle,
            None => true,
        }
    }
}

/// Réduit une image à une grille de luminances moyennées.
///
/// La grille a des dimensions FIXES, quelle que soit la résolution ou le
/// rapport d'aspect de la caméra : le filtre compare des proportions de
/// cases, pas des pixels, et deux grilles doivent être comparables d'une
/// frame à l'autre.
fn downscale_luma(image: &RgbImage) -> Vec<u8> {
    let (width, height) = (image.width(), image.height());

    if width == 0 || height == 0 {
        return vec![0; GRID_WIDTH * GRID_HEIGHT];
    }

    let mut grid = Vec::with_capacity(GRID_WIDTH * GRID_HEIGHT);

    for cell_y in 0..GRID_HEIGHT as u32 {
        for cell_x in 0..GRID_WIDTH as u32 {
            let mut total = 0u32;
            let mut samples = 0u32;

            for sample_y in 0..SAMPLES_PER_CELL {
                for sample_x in 0..SAMPLES_PER_CELL {
                    // Position du point d'échantillonnage, en coordonnées
                    // d'image. Le `+ 1` et le dénominateur `SAMPLES + 1`
                    // répartissent les points à l'INTÉRIEUR de la case plutôt
                    // que sur son bord, où ils déborderaient sur la voisine.
                    let x = (cell_x * (SAMPLES_PER_CELL + 1) + sample_x + 1) * width
                        / (GRID_WIDTH as u32 * (SAMPLES_PER_CELL + 1));
                    let y = (cell_y * (SAMPLES_PER_CELL + 1) + sample_y + 1) * height
                        / (GRID_HEIGHT as u32 * (SAMPLES_PER_CELL + 1));

                    let pixel = image.get_pixel(x.min(width - 1), y.min(height - 1));

                    // Luminance approchée : les coefficients BT.601 à
                    // l'entier près. La précision n'a aucune importance ici,
                    // seule compte la VARIATION d'une frame à l'autre.
                    total += (u32::from(pixel[0]) * 77
                        + u32::from(pixel[1]) * 150
                        + u32::from(pixel[2]) * 29)
                        >> 8;
                    samples += 1;
                }
            }

            grid.push((total / samples.max(1)) as u8);
        }
    }

    grid
}

/// Proportion de cases dont la luminance s'écarte du fond de plus de
/// `threshold`.
fn changed_ratio(background: &[f32], current: &[u8], threshold: u8) -> f32 {
    if background.len() != current.len() || current.is_empty() {
        // Deux grilles de tailles différentes ne sont pas comparables : on
        // signale un changement total plutôt que de comparer n'importe quoi.
        return 1.0;
    }

    let threshold = f32::from(threshold);
    let changed = background
        .iter()
        .zip(current)
        .filter(|&(&background, &current)| (f32::from(current) - background).abs() > threshold)
        .count();

    changed as f32 / current.len() as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> MotionConfig {
        MotionConfig {
            enabled: true,
            pixel_threshold: 20,
            min_changed_ratio: 0.01,
            hold_secs: 3,
            max_idle_secs: 20,
            background_time_constant_secs: 5.0,
        }
    }

    /// Fond = frame précédente : isole les garde-fous temporels de
    /// l'absorption progressive par le fond.
    fn frame_to_frame() -> MotionConfig {
        MotionConfig {
            background_time_constant_secs: 0.0,
            ..config()
        }
    }

    fn background(grid: &[u8]) -> Vec<f32> {
        grid.iter().map(|&value| f32::from(value)).collect()
    }

    fn uniform(level: u8) -> RgbImage {
        RgbImage::from_pixel(320, 240, image::Rgb([level, level, level]))
    }

    /// Image uniforme avec un rectangle plus clair : une « personne ».
    fn with_subject(level: u8, subject_x: u32, subject_width: u32) -> RgbImage {
        RgbImage::from_fn(320, 240, |x, y| {
            if x >= subject_x && x < subject_x + subject_width && (60..200).contains(&y) {
                image::Rgb([240, 240, 240])
            } else {
                image::Rgb([level, level, level])
            }
        })
    }

    // --- Réduction et comparaison ---

    #[test]
    fn the_grid_has_the_documented_fixed_size() {
        // Des grilles de tailles différentes ne seraient pas comparables
        // d'une frame à l'autre.
        assert_eq!(
            downscale_luma(&uniform(100)).len(),
            GRID_WIDTH * GRID_HEIGHT
        );
        assert_eq!(
            downscale_luma(&RgbImage::from_pixel(1920, 1080, image::Rgb([5, 5, 5]))).len(),
            GRID_WIDTH * GRID_HEIGHT
        );
    }

    #[test]
    fn a_uniform_image_reduces_to_a_uniform_grid() {
        let grid = downscale_luma(&uniform(128));

        for (index, &value) in grid.iter().enumerate() {
            assert!(
                value.abs_diff(128) <= 1,
                "case {index} : {value} au lieu de ~128"
            );
        }
    }

    #[test]
    fn a_one_pixel_image_does_not_panic() {
        // Pas de division par zéro ni de débordement sur une image plus
        // petite que la grille.
        let tiny = RgbImage::from_pixel(1, 1, image::Rgb([200, 200, 200]));
        assert_eq!(downscale_luma(&tiny).len(), GRID_WIDTH * GRID_HEIGHT);
    }

    #[test]
    fn two_identical_grids_have_not_changed() {
        let grid = downscale_luma(&uniform(100));
        assert_eq!(changed_ratio(&background(&grid), &grid, 20), 0.0);
    }

    #[test]
    fn a_change_below_the_threshold_is_not_counted() {
        // C'est ce qui rend le filtre insensible au bruit de capteur.
        let before = downscale_luma(&uniform(100));
        let after = downscale_luma(&uniform(110));

        assert_eq!(changed_ratio(&background(&before), &after, 20), 0.0);
    }

    #[test]
    fn a_change_above_the_threshold_is_counted_everywhere_it_happened() {
        let before = downscale_luma(&uniform(60));
        let after = downscale_luma(&uniform(200));

        assert_eq!(changed_ratio(&background(&before), &after, 20), 1.0);
    }

    #[test]
    fn incomparable_grids_report_a_total_change() {
        assert_eq!(changed_ratio(&[1.0, 2.0, 3.0], &[1, 2], 20), 1.0);
        assert_eq!(changed_ratio(&[], &[], 20), 1.0);
    }

    // --- Verdicts du filtre ---

    #[test]
    fn the_very_first_frame_is_always_scanned() {
        // Sans référence, il n'y a pas de verdict possible — et la scène
        // déjà en place au démarrage ne doit pas être ignorée.
        let mut gate = MotionGate::new(config());
        let verdict = gate.evaluate(&uniform(100));

        assert!(verdict.scan);
        assert_eq!(verdict.reason, ScanReason::NoReference);
    }

    #[test]
    fn a_still_scene_stops_being_scanned() {
        // LA raison d'être de tout ce module.
        let mut gate = MotionGate::new(config());
        let start = Instant::now();

        gate.evaluate_at(&uniform(100), start);
        // La deuxième frame identique reste dans la rémanence du premier
        // passage ; c'est la suivante, au-delà de `hold_secs`, qui doit être
        // écartée.
        gate.evaluate_at(&uniform(100), start + Duration::from_secs(4));

        let verdict = gate.evaluate_at(&uniform(100), start + Duration::from_secs(8));
        assert!(!verdict.scan, "ratio {}", verdict.changed_ratio);
    }

    #[test]
    fn someone_walking_into_the_frame_is_scanned() {
        let mut gate = MotionGate::new(config());
        let start = Instant::now();

        gate.evaluate_at(&uniform(100), start);

        let verdict = gate.evaluate_at(&with_subject(100, 40, 60), start + Duration::from_secs(8));

        assert!(verdict.scan);
        assert_eq!(verdict.reason, ScanReason::Motion);
    }

    #[test]
    fn a_distant_subject_still_trips_the_filter_at_the_default_ratio() {
        // Le réglage par défaut doit voir une silhouette qui n'occupe qu'une
        // petite part du champ, sinon le filtre coûte des détections.
        let mut gate = MotionGate::new(MotionConfig::default());
        let start = Instant::now();

        gate.evaluate_at(&uniform(100), start);

        // 16 pixels de large sur 320, soit 5 % de la largeur.
        let verdict =
            gate.evaluate_at(&with_subject(100, 150, 16), start + Duration::from_secs(60));

        assert!(
            verdict.scan && verdict.reason == ScanReason::Motion,
            "ratio {} pour une silhouette lointaine",
            verdict.changed_ratio
        );
    }

    #[test]
    fn scanning_continues_while_someone_may_have_merely_stopped() {
        // PREMIER GARDE-FOU : quelqu'un d'immobile est toujours là.
        let mut gate = MotionGate::new(frame_to_frame());
        let start = Instant::now();

        gate.evaluate_at(&uniform(100), start);
        gate.evaluate_at(&with_subject(100, 40, 60), start + Duration::from_secs(1));

        // La personne ne bouge plus : l'image est identique à la précédente.
        let verdict = gate.evaluate_at(&with_subject(100, 40, 60), start + Duration::from_secs(2));

        assert!(verdict.scan);
        assert_eq!(verdict.reason, ScanReason::AfterMotion);
        assert_eq!(verdict.changed_ratio, 0.0, "l'image n'a pas changé");
    }

    #[test]
    fn the_motion_hold_eventually_expires() {
        let mut gate = MotionGate::new(frame_to_frame());
        let start = Instant::now();

        gate.evaluate_at(&uniform(100), start);
        gate.evaluate_at(&with_subject(100, 40, 60), start + Duration::from_secs(1));

        // `hold_secs` vaut 3 : à +5 s, la rémanence a expiré.
        let verdict = gate.evaluate_at(&with_subject(100, 40, 60), start + Duration::from_secs(5));

        assert!(!verdict.scan);
    }

    #[test]
    fn a_periodic_scan_happens_even_in_a_perfectly_still_scene() {
        // SECOND GARDE-FOU : une présence déjà immobile au moment où le
        // filtre prend sa référence ne produit AUCUN mouvement. Sans ce
        // passage, elle ne serait jamais détectée.
        let mut gate = MotionGate::new(config());
        let start = Instant::now();

        gate.evaluate_at(&uniform(100), start);
        gate.evaluate_at(&uniform(100), start + Duration::from_secs(5));

        // `max_idle_secs` vaut 20, comptés depuis le dernier passage.
        let verdict = gate.evaluate_at(&uniform(100), start + Duration::from_secs(26));

        assert!(verdict.scan);
        assert_eq!(verdict.reason, ScanReason::Periodic);
    }

    #[test]
    fn a_max_idle_of_zero_disables_the_periodic_scan() {
        let mut gate = MotionGate::new(MotionConfig {
            max_idle_secs: 0,
            ..config()
        });
        let start = Instant::now();

        gate.evaluate_at(&uniform(100), start);

        let verdict = gate.evaluate_at(&uniform(100), start + Duration::from_secs(3600));
        assert!(!verdict.scan);
    }

    #[test]
    fn a_disabled_filter_always_scans() {
        // Rétablit exactement le comportement antérieur au pré-filtre.
        let mut gate = MotionGate::new(MotionConfig {
            enabled: false,
            ..config()
        });
        let start = Instant::now();

        for seconds in 0..5 {
            let verdict =
                gate.evaluate_at(&uniform(100), start + Duration::from_secs(seconds * 100));
            assert!(verdict.scan, "passage {seconds}");
        }
    }

    #[test]
    fn a_gradual_light_change_does_not_trip_the_filter() {
        // Le jour qui se lève fait dériver la luminance de quelques unités
        // entre deux frames : ce n'est pas du mouvement.
        let mut gate = MotionGate::new(config());
        let start = Instant::now();

        gate.evaluate_at(&uniform(100), start);
        // Au-delà de `hold_secs` pour sortir de la rémanence du premier
        // passage.
        gate.evaluate_at(&uniform(100), start + Duration::from_secs(5));

        let verdict = gate.evaluate_at(&uniform(108), start + Duration::from_secs(10));

        assert!(
            !verdict.scan,
            "une dérive de 8 niveaux a été prise pour du mouvement (ratio {})",
            verdict.changed_ratio
        );
    }

    // --- Fond glissant ---

    #[test]
    fn someone_who_stops_still_stands_out_from_the_background() {
        // Contre la frame précédente, une personne qui s'arrête ne « bouge »
        // plus ; contre le fond, elle s'en écarte toujours.
        let mut gate = MotionGate::new(config());
        let start = Instant::now();
        let scene = with_subject(100, 40, 60);

        gate.evaluate_at(&uniform(100), start);
        gate.evaluate_at(&scene, start + Duration::from_secs(1));

        // Au-delà de `hold_secs` : seul l'écart au fond peut ouvrir le filtre.
        let verdict = gate.evaluate_at(&scene, start + Duration::from_secs(5));

        assert!(verdict.scan);
        assert_eq!(verdict.reason, ScanReason::Motion);
    }

    #[test]
    fn a_subject_at_rest_is_eventually_absorbed_by_the_background() {
        // Le fond suit la scène : une fois la personne immobile intégrée au
        // décor, le filtre se referme.
        let mut gate = MotionGate::new(MotionConfig {
            max_idle_secs: 0,
            ..config()
        });
        let start = Instant::now();
        let scene = with_subject(100, 40, 60);

        gate.evaluate_at(&uniform(100), start);

        let mut verdict = gate.evaluate_at(&scene, start);
        for quarter in 1..=120 {
            verdict = gate.evaluate_at(&scene, start + Duration::from_millis(quarter * 250));
        }

        // 30 s, soit six constantes de temps.
        assert!(!verdict.scan, "ratio {}", verdict.changed_ratio);
    }

    #[test]
    fn a_slow_change_missed_frame_to_frame_is_seen_against_the_background() {
        // Une silhouette qui émerge lentement : 8 niveaux par évaluation,
        // sous le seuil de 20 d'une frame à la suivante.
        let emerging = |level: u8| {
            RgbImage::from_fn(320, 240, |x, y| {
                if (40..100).contains(&x) && (60..200).contains(&y) {
                    image::Rgb([level, level, level])
                } else {
                    image::Rgb([100, 100, 100])
                }
            })
        };

        for (config, expected) in [(frame_to_frame(), false), (config(), true)] {
            let mut gate = MotionGate::new(MotionConfig {
                max_idle_secs: 0,
                ..config
            });
            let start = Instant::now();

            gate.evaluate_at(&uniform(100), start);
            gate.evaluate_at(&uniform(100), start + Duration::from_secs(10));

            let mut seen = false;
            for step in 1..=10u8 {
                let verdict = gate.evaluate_at(
                    &emerging(100 + step * 8),
                    start + Duration::from_secs(10) + Duration::from_millis(u64::from(step) * 250),
                );
                seen |= verdict.reason == ScanReason::Motion && verdict.scan;
            }

            assert_eq!(
                seen, expected,
                "constante {}",
                config.background_time_constant_secs
            );
        }
    }
}
