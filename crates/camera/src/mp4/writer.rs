//! Écriture d'un fichier MP4 **fragmenté** (fMP4) contenant une piste H.264.
//!
//! # Pourquoi fragmenté
//!
//! Un MP4 classique place sa table des matières (`moov`) à la fin, une fois
//! toutes les tailles connues. C'est rédhibitoire ici : un enregistrement de
//! vidéosurveillance peut être interrompu par une coupure de courant, un
//! disque plein ou un redémarrage, et un MP4 classique amputé de sa fin n'est
//! pas « un peu abîmé » — il est **entièrement illisible**, pas une image ne
//! peut en être tirée.
//!
//! Le MP4 fragmenté écrit au contraire un segment d'initialisation complet
//! dès l'ouverture, puis des fragments autonomes. Un fichier tronqué perd son
//! dernier fragment, et rien d'autre.
//!
//! # Structure produite
//!
//! ```text
//! ftyp                      déclaration des marques de compatibilité
//! moov                      segment d'initialisation
//!   mvhd                    en-tête du film
//!   trak > ... > avcC       la piste, et les paramètres H.264
//!   mvex > trex             « des fragments suivent »
//! moof + mdat               un fragment : ses métadonnées, puis ses images
//! moof + mdat               ...
//! ```
//!
//! # Un fragment par groupe d'images
//!
//! Chaque fragment commence à une image clé et court jusqu'à la suivante.
//! C'est le découpage naturel : une image clé est le seul point d'entrée d'un
//! décodeur, donc le seul endroit où un fragment peut commencer sans être
//! tributaire du précédent.
//!
//! Il impose de garder un groupe d'images en mémoire avant de l'écrire : les
//! tailles et les durées de ses images doivent figurer dans l'en-tête du
//! fragment, qui les précède. À deux secondes d'intervalle entre images clés,
//! cela représente quelques centaines de kilo-octets.

use std::fs::File;
use std::io::{self, Seek, SeekFrom, Write};
use std::path::Path;

use super::boxes::{self, Mp4Box};
use crate::h264::{NAL_PPS, NAL_SPS, nal_type};

/// Base de temps de la piste, en unités par seconde.
///
/// 90 kHz : la même que l'horloge RTP du flux (voir `crate::rtsp::rtp`), ce
/// qui évite toute conversion entre ce qui est diffusé et ce qui est écrit.
pub const TIMESCALE: u32 = 90_000;

/// Identifiant de la piste vidéo. Il n'y en a qu'une — pas d'audio.
const TRACK_ID: u32 = 1;

/// Drapeaux de `trun` : décalage des données, puis durée, taille et
/// propriétés par image.
///
/// Les durées sont déclarées IMAGE PAR IMAGE, et non par une valeur unique
/// dans `trex` : la cadence réelle d'une caméra n'est pas constante (une
/// webcam UVC la réduit en basse luminosité), et c'est précisément le défaut
/// que corrigeait déjà le format d'enregistrement horodaté.
const TRUN_FLAGS: u32 = 0x0001 | 0x0100 | 0x0200 | 0x0400;

/// Propriétés d'une image clé : les suivantes peuvent dépendre d'elle, et un
/// lecteur peut y commencer.
const SAMPLE_FLAGS_KEYFRAME: u32 = 0x0200_0000;

/// Propriétés d'une image intermédiaire : elle dépend d'une autre, et un
/// lecteur ne peut pas y commencer.
const SAMPLE_FLAGS_DELTA: u32 = 0x0101_0000;

/// Une image en attente d'écriture dans le fragment courant.
struct Sample {
    /// Les NAL au format AVCC (longueur de 32 bits puis données), telles
    /// qu'elles iront dans le `mdat`.
    data: Vec<u8>,
    /// Durée d'affichage, en unités de [`TIMESCALE`].
    duration: u32,
    keyframe: bool,
}

/// Emplacements, dans le fichier, des champs de durée à corriger à la
/// fermeture.
///
/// Un MP4 fragmenté déclare une durée NULLE à l'ouverture : elle n'est pas
/// connue, et c'est même ainsi qu'un lecteur reconnaît un flux dont la fin
/// n'est pas écrite d'avance. Mais une fois l'enregistrement terminé, la
/// laisser à zéro oblige chaque lecteur à la deviner en parcourant les
/// fragments — avec, en attendant, une durée approximative et une barre de
/// progression qui saute.
///
/// On revient donc l'inscrire. Le fichier reste lisible s'il n'y a jamais
/// d'écriture finale (coupure de courant) : il retombe simplement dans le cas
/// « durée inconnue », celui dans lequel il a vécu jusque-là.
struct DurationFields {
    movie: u64,
    track: u64,
    media: u64,
}

/// Écrivain d'un fichier MP4 fragmenté.
pub struct Fmp4Writer {
    file: File,
    name: String,
    durations: DurationFields,
    /// Durée totale écrite, en unités de [`TIMESCALE`].
    total_duration: u64,

    /// Images du fragment en cours, en attente de leur en-tête.
    pending: Vec<Sample>,
    /// Horodatage de la première image du fragment en cours.
    fragment_start: u64,
    /// Horodatage de la dernière image soumise, pour en déduire les durées.
    last_timestamp: Option<u64>,
    /// Numéro du prochain fragment. Les lecteurs n'exigent pas qu'il soit
    /// exact, mais une suite cohérente aide au diagnostic.
    sequence: u32,
    /// Durée nominale d'une image, utilisée pour la DERNIÈRE de chaque
    /// fragment — la seule dont on ne connaîtra jamais l'écart avec la
    /// suivante au moment de l'écrire.
    nominal_duration: u32,
}

impl Fmp4Writer {
    /// Crée un fichier et y écrit le segment d'initialisation.
    ///
    /// `sps` et `pps` viennent de la première image clé : ce sont eux qui
    /// décrivent le flux au décodeur (profil, niveau, dimensions réelles).
    /// Sans eux, le fichier n'est pas lisible — d'où leur présence dès la
    /// construction plutôt qu'à la première image.
    pub fn create(
        path: &Path,
        name: String,
        width: u32,
        height: u32,
        fps: u32,
        sps: &[u8],
        pps: &[u8],
    ) -> io::Result<Self> {
        let mut file = File::options().write(true).create_new(true).open(path)?;

        let mut header = Vec::new();
        write_ftyp(&mut header);
        let durations = write_moov(&mut header, width, height, sps, pps);
        file.write_all(&header)?;

        Ok(Self {
            file,
            name,
            durations,
            total_duration: 0,
            pending: Vec::new(),
            fragment_start: 0,
            last_timestamp: None,
            sequence: 1,
            nominal_duration: TIMESCALE / fps.clamp(1, 240),
        })
    }

    /// Nom du fichier, tel qu'attendu par les routes de la caméra.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Ajoute une image au fichier.
    ///
    /// `nals` sont les unités NAL de l'image, sans préfixe de délimitation
    /// (telles que les livre `crate::h264`). `timestamp` est en unités de
    /// [`TIMESCALE`], et doit croître.
    ///
    /// L'écriture n'a pas lieu immédiatement : l'image rejoint le fragment en
    /// cours, qui part sur disque à la prochaine image clé (voir la note en
    /// tête de module).
    pub fn write_frame(
        &mut self,
        nals: &[Vec<u8>],
        timestamp: u64,
        keyframe: bool,
    ) -> io::Result<()> {
        // Une image clé ferme le fragment précédent : c'est le seul endroit
        // où le suivant peut commencer.
        if keyframe && !self.pending.is_empty() {
            self.flush()?;
        }

        // La durée d'une image est l'écart jusqu'à la SUIVANTE. On ne la
        // connaît donc qu'en recevant celle-ci : on corrige rétroactivement
        // la précédente, et la dernière du fragment gardera la durée
        // nominale.
        if let Some(previous) = self.last_timestamp
            && let Some(last) = self.pending.last_mut()
        {
            last.duration = u32::try_from(timestamp.saturating_sub(previous))
                .unwrap_or(self.nominal_duration)
                .max(1);
        }

        if self.pending.is_empty() {
            self.fragment_start = timestamp;
        }

        self.pending.push(Sample {
            data: to_avcc(nals),
            duration: self.nominal_duration,
            keyframe,
        });
        self.last_timestamp = Some(timestamp);

        Ok(())
    }

    /// Écrit le fragment en cours, s'il n'est pas vide.
    fn flush(&mut self) -> io::Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }

        let mut fragment = Vec::new();
        write_fragment(
            &mut fragment,
            self.sequence,
            self.fragment_start,
            &self.pending,
        );

        self.file.write_all(&fragment)?;

        self.total_duration += self
            .pending
            .iter()
            .map(|sample| u64::from(sample.duration))
            .sum::<u64>();

        self.sequence += 1;
        self.pending.clear();

        Ok(())
    }

    /// Inscrit la durée totale dans l'en-tête, maintenant qu'elle est connue.
    fn write_duration(&mut self) -> io::Result<()> {
        let duration = self.total_duration.to_be_bytes();

        for offset in [
            self.durations.movie,
            self.durations.track,
            self.durations.media,
        ] {
            self.file.seek(SeekFrom::Start(offset))?;
            self.file.write_all(&duration)?;
        }

        self.file.seek(SeekFrom::End(0))?;
        Ok(())
    }

    /// Referme le fichier en écrivant le dernier fragment.
    ///
    /// À appeler explicitement : `Drop` ne peut pas signaler une erreur
    /// d'écriture, et perdre les dernières secondes d'un enregistrement sans
    /// qu'aucun message ne le dise serait le pire des deux mondes.
    pub fn finish(mut self) -> io::Result<()> {
        self.flush()?;
        self.write_duration()?;
        self.file.flush()
    }
}

/// Convertit des NAL en format AVCC : chacune précédée de sa longueur sur
/// 32 bits, comme l'exige le `mdat` d'un MP4.
///
/// Les SPS et PPS sont ÉCARTÉS : ils sont déjà déclarés une fois pour toutes
/// dans la boîte `avcC` du segment d'initialisation, et les répéter dans
/// chaque image clé ne ferait que grossir le fichier. C'est l'identifiant
/// constant des jeux de paramètres qui rend cette économie sûre (voir
/// `crate::h264::encoder`).
fn to_avcc(nals: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();

    for nal in nals {
        if matches!(nal_type(nal), Some(NAL_SPS) | Some(NAL_PPS)) {
            continue;
        }

        boxes::u32(&mut out, nal.len() as u32);
        out.extend_from_slice(nal);
    }

    out
}

/// `ftyp` : les spécifications auxquelles le fichier se conforme.
fn write_ftyp(out: &mut Vec<u8>) {
    let boxed = Mp4Box::start(out, b"ftyp");

    out.extend_from_slice(b"isom");
    boxes::u32(out, 512);
    // `iso5` annonce le MP4 fragmenté, `avc1` le codec : les lecteurs s'en
    // servent pour refuser tôt un fichier qu'ils ne sauraient pas lire.
    for brand in [b"isom", b"iso2", b"iso5", b"avc1", b"mp41"] {
        out.extend_from_slice(brand);
    }

    boxed.end(out);
}

/// `moov` : le segment d'initialisation, écrit dès l'ouverture du fichier.
///
/// Retourne l'emplacement des trois champs de durée, à corriger à la
/// fermeture (voir [`DurationFields`]).
fn write_moov(
    out: &mut Vec<u8>,
    width: u32,
    height: u32,
    sps: &[u8],
    pps: &[u8],
) -> DurationFields {
    let moov = Mp4Box::start(out, b"moov");

    let movie = write_mvhd(out);
    let (track, media) = write_trak(out, width, height, sps, pps);
    write_mvex(out);

    moov.end(out);

    DurationFields {
        movie,
        track,
        media,
    }
}

/// `mvhd` : en-tête du film. Retourne l'emplacement de son champ de durée.
///
/// Version 1 : dates et durée sur 64 bits. En 32 bits, la durée déborderait
/// au bout de treize heures d'enregistrement continu — un cas rare, mais qui
/// se solderait par une durée absurde plutôt que par une erreur.
fn write_mvhd(out: &mut Vec<u8>) -> u64 {
    let boxed = Mp4Box::start_full(out, b"mvhd", 1, 0);

    boxes::u64(out, 0); // création
    boxes::u64(out, 0); // modification
    boxes::u32(out, TIMESCALE);

    // Durée NULLE à l'ouverture : elle n'est pas connue, et c'est ainsi qu'un
    // lecteur reconnaît un flux fragmenté. Corrigée à la fermeture.
    let duration_offset = out.len() as u64;
    boxes::u64(out, 0);

    boxes::u32(out, 0x0001_0000); // vitesse de lecture : 1.0
    boxes::u16(out, 0x0100); // volume : 1.0
    boxes::zeros(out, 2 + 8); // réservé
    boxes::identity_matrix(out);
    boxes::zeros(out, 24); // réservé
    boxes::u32(out, TRACK_ID + 1); // prochain identifiant de piste libre

    boxed.end(out);
    duration_offset
}

/// `trak` : la piste vidéo et tout ce qui la décrit.
///
/// Retourne l'emplacement des champs de durée de `tkhd` et de `mdhd`.
fn write_trak(out: &mut Vec<u8>, width: u32, height: u32, sps: &[u8], pps: &[u8]) -> (u64, u64) {
    let trak = Mp4Box::start(out, b"trak");

    // `tkhd`, drapeaux 3 : piste activée et utilisée dans la présentation.
    let tkhd = Mp4Box::start_full(out, b"tkhd", 1, 3);
    boxes::u64(out, 0); // création
    boxes::u64(out, 0); // modification
    boxes::u32(out, TRACK_ID);
    boxes::zeros(out, 4); // réservé
    let track_duration_offset = out.len() as u64;
    boxes::u64(out, 0); // durée, corrigée à la fermeture
    boxes::zeros(out, 8); // réservé
    boxes::u16(out, 0); // calque
    boxes::u16(out, 0); // groupe alternatif
    boxes::u16(out, 0); // volume : nul pour une piste vidéo
    boxes::zeros(out, 2); // réservé
    boxes::identity_matrix(out);
    // Dimensions d'AFFICHAGE, en virgule fixe 16.16.
    boxes::u32(out, width << 16);
    boxes::u32(out, height << 16);
    tkhd.end(out);

    let mdia = Mp4Box::start(out, b"mdia");

    let mdhd = Mp4Box::start_full(out, b"mdhd", 1, 0);
    boxes::u64(out, 0); // création
    boxes::u64(out, 0); // modification
    boxes::u32(out, TIMESCALE);
    let media_duration_offset = out.len() as u64;
    boxes::u64(out, 0); // durée, corrigée à la fermeture
    // Langue « und » (indéterminée), codée sur 5 bits par caractère.
    boxes::u16(out, 0x55C4);
    boxes::u16(out, 0); // qualité
    mdhd.end(out);

    let hdlr = Mp4Box::start_full(out, b"hdlr", 0, 0);
    boxes::u32(out, 0); // prédéfini
    out.extend_from_slice(b"vide"); // piste vidéo
    boxes::zeros(out, 12); // réservé
    out.extend_from_slice(b"FoxGuard\0");
    hdlr.end(out);

    let minf = Mp4Box::start(out, b"minf");

    let vmhd = Mp4Box::start_full(out, b"vmhd", 0, 1);
    boxes::u16(out, 0); // mode de composition
    boxes::zeros(out, 6); // couleur d'arrière-plan
    vmhd.end(out);

    // `dinf`/`dref` : où sont les données. Le drapeau 1 de `url ` signifie
    // « dans ce fichier même », ce qui est le cas et évite toute référence
    // externe.
    let dinf = Mp4Box::start(out, b"dinf");
    let dref = Mp4Box::start_full(out, b"dref", 0, 0);
    boxes::u32(out, 1); // une entrée
    Mp4Box::start_full(out, b"url ", 0, 1).end(out);
    dref.end(out);
    dinf.end(out);

    write_stbl(out, width, height, sps, pps);

    minf.end(out);
    mdia.end(out);
    trak.end(out);

    (track_duration_offset, media_duration_offset)
}

/// `stbl` : les tables d'échantillons.
///
/// Toutes VIDES, et c'est normal : dans un MP4 fragmenté, ces informations
/// vivent dans les fragments. Les boîtes doivent néanmoins être présentes,
/// faute de quoi les lecteurs rejettent la piste.
fn write_stbl(out: &mut Vec<u8>, width: u32, height: u32, sps: &[u8], pps: &[u8]) {
    let stbl = Mp4Box::start(out, b"stbl");

    let stsd = Mp4Box::start_full(out, b"stsd", 0, 0);
    boxes::u32(out, 1); // une description : la nôtre
    write_avc1(out, width, height, sps, pps);
    stsd.end(out);

    for kind in [b"stts", b"stsc", b"stco"] {
        let boxed = Mp4Box::start_full(out, kind, 0, 0);
        boxes::u32(out, 0); // aucune entrée
        boxed.end(out);
    }

    let stsz = Mp4Box::start_full(out, b"stsz", 0, 0);
    boxes::u32(out, 0); // taille d'échantillon non constante
    boxes::u32(out, 0); // aucune entrée
    stsz.end(out);

    stbl.end(out);
}

/// `avc1` : la description de l'échantillon vidéo, et la configuration H.264
/// qu'elle contient.
fn write_avc1(out: &mut Vec<u8>, width: u32, height: u32, sps: &[u8], pps: &[u8]) {
    let avc1 = Mp4Box::start(out, b"avc1");

    boxes::zeros(out, 6); // réservé
    boxes::u16(out, 1); // index de référence de données
    boxes::zeros(out, 2 + 2 + 12); // prédéfini et réservé
    boxes::u16(out, width as u16);
    boxes::u16(out, height as u16);
    boxes::u32(out, 0x0048_0000); // résolution horizontale : 72 ppp
    boxes::u32(out, 0x0048_0000); // résolution verticale : 72 ppp
    boxes::u32(out, 0); // réservé
    boxes::u16(out, 1); // nombre d'images par échantillon
    boxes::zeros(out, 32); // nom du compresseur, laissé vide
    boxes::u16(out, 0x0018); // profondeur : 24 bits
    boxes::u16(out, 0xFFFF); // table de couleurs : aucune

    write_avcc(out, sps, pps);

    avc1.end(out);
}

/// `avcC` : la configuration du décodeur H.264.
///
/// C'est la boîte qui porte les jeux de paramètres. Un lecteur refuse la
/// piste sans elle — et c'est pourquoi les SPS/PPS doivent être connus dès la
/// création du fichier.
fn write_avcc(out: &mut Vec<u8>, sps: &[u8], pps: &[u8]) {
    let boxed = Mp4Box::start(out, b"avcC");

    out.push(1); // version de configuration
    // Profil, compatibilité et niveau : lus dans le SPS, qu'ils décrivent.
    out.push(sps.get(1).copied().unwrap_or(0x42));
    out.push(sps.get(2).copied().unwrap_or(0));
    out.push(sps.get(3).copied().unwrap_or(0x1E));
    // 6 bits à 1, puis la taille des préfixes de longueur moins un : 3, donc
    // des longueurs sur 4 octets (voir `to_avcc`).
    out.push(0xFF);
    // 3 bits à 1, puis le nombre de SPS : un seul.
    out.push(0xE1);
    boxes::u16(out, sps.len() as u16);
    out.extend_from_slice(sps);
    out.push(1); // un seul PPS
    boxes::u16(out, pps.len() as u16);
    out.extend_from_slice(pps);

    boxed.end(out);
}

/// `mvex` : annonce que la durée réelle vit dans les fragments.
///
/// Sans elle, un lecteur s'en tient au `moov` — qui ne décrit aucune image —
/// et conclut que le fichier est vide.
fn write_mvex(out: &mut Vec<u8>) {
    let mvex = Mp4Box::start(out, b"mvex");

    let trex = Mp4Box::start_full(out, b"trex", 0, 0);
    boxes::u32(out, TRACK_ID);
    boxes::u32(out, 1); // index de description d'échantillon
    boxes::u32(out, 0); // durée par défaut : aucune, elles sont par image
    boxes::u32(out, 0); // taille par défaut : idem
    boxes::u32(out, 0); // propriétés par défaut : idem
    trex.end(out);

    mvex.end(out);
}

/// Un fragment : `moof` (ses métadonnées) suivi de `mdat` (ses images).
fn write_fragment(out: &mut Vec<u8>, sequence: u32, start: u64, samples: &[Sample]) {
    let moof = Mp4Box::start(out, b"moof");
    let moof_offset = moof.offset();

    let mfhd = Mp4Box::start_full(out, b"mfhd", 0, 0);
    boxes::u32(out, sequence);
    mfhd.end(out);

    let traf = Mp4Box::start(out, b"traf");

    // `tfhd`, drapeau 0x020000 : les décalages sont comptés depuis le début
    // de ce `moof`. C'est le mode que tous les lecteurs modernes attendent,
    // et le seul qui reste juste si le fragment est déplacé dans le fichier.
    let tfhd = Mp4Box::start_full(out, b"tfhd", 0, 0x02_0000);
    boxes::u32(out, TRACK_ID);
    tfhd.end(out);

    // `tfdt` version 1 : l'horodatage de début sur 64 bits. En 32 bits, un
    // enregistrement continu déborderait au bout de treize heures.
    let tfdt = Mp4Box::start_full(out, b"tfdt", 1, 0);
    boxes::u64(out, start);
    tfdt.end(out);

    let trun = Mp4Box::start_full(out, b"trun", 0, TRUN_FLAGS);
    boxes::u32(out, samples.len() as u32);

    // Emplacement du décalage vers les données, renseigné plus bas : il
    // dépend de la taille totale du `moof`, qu'on ne connaîtra qu'une fois
    // toutes les images décrites.
    let data_offset_position = out.len();
    boxes::u32(out, 0);

    for sample in samples {
        boxes::u32(out, sample.duration);
        boxes::u32(out, sample.data.len() as u32);
        boxes::u32(
            out,
            if sample.keyframe {
                SAMPLE_FLAGS_KEYFRAME
            } else {
                SAMPLE_FLAGS_DELTA
            },
        );
    }

    trun.end(out);
    traf.end(out);
    moof.end(out);

    // Les données commencent juste après l'en-tête du `mdat` qui suit, soit
    // huit octets après la fin du `moof`.
    let data_offset = (out.len() - moof_offset + 8) as u32;
    out[data_offset_position..data_offset_position + 4].copy_from_slice(&data_offset.to_be_bytes());

    let mdat = Mp4Box::start(out, b"mdat");
    for sample in samples {
        out.extend_from_slice(&sample.data);
    }
    mdat.end(out);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SPS et PPS plausibles : profil 0x42 (Baseline), contraintes 0xC0,
    /// niveau 0x1E (3.0).
    const SPS: [u8; 6] = [0x67, 0x42, 0xC0, 0x1E, 0xAB, 0xCD];
    const PPS: [u8; 4] = [0x68, 0xCE, 0x3C, 0x80];

    /// Une boîte trouvée dans un fichier : son type et ses bornes.
    #[derive(Debug, PartialEq, Eq)]
    struct Found {
        kind: String,
        start: usize,
        end: usize,
    }

    /// Parcourt les boîtes d'un niveau, à partir de `offset`.
    ///
    /// Relire le fichier plutôt que vérifier des octets à des positions
    /// écrites en dur : une erreur de taille déplacerait TOUT ce qui suit, et
    /// des assertions sur des positions fixes la rateraient en bloc.
    fn boxes_in(data: &[u8], mut offset: usize, end: usize) -> Vec<Found> {
        let mut found = Vec::new();

        while offset + 8 <= end {
            let size = u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
            let kind = String::from_utf8_lossy(&data[offset + 4..offset + 8]).to_string();

            assert!(size >= 8, "boîte {kind} de taille absurde : {size}");
            assert!(
                offset + size <= end,
                "boîte {kind} qui déborde de son parent ({} > {end})",
                offset + size
            );

            found.push(Found {
                kind,
                start: offset,
                end: offset + size,
            });
            offset += size;
        }

        assert_eq!(offset, end, "des octets traînent après la dernière boîte");
        found
    }

    /// Les boîtes de premier niveau d'un fichier.
    fn top_level(data: &[u8]) -> Vec<Found> {
        boxes_in(data, 0, data.len())
    }

    /// Décalage, depuis le début d'une boîte, auquel commencent ses enfants.
    ///
    /// Huit octets d'en-tête pour un conteneur ordinaire — mais deux boîtes
    /// du chemin qui mène à `avcC` portent leur propre contenu AVANT leurs
    /// enfants, et l'ignorer ferait lire n'importe quoi :
    ///
    /// - `stsd` est une « full box » suivie d'un nombre d'entrées ;
    /// - `avc1` est une description d'échantillon visuel, dont les 78 octets
    ///   décrivent la vidéo (dimensions, profondeur, résolution).
    fn children_offset(kind: &str) -> usize {
        match kind {
            "stsd" => 8 + 4 + 4,
            "avc1" => 8 + 78,
            _ => 8,
        }
    }

    /// Retrouve une boîte par son chemin, ex. `["moov", "trak", "mdia"]`.
    fn find(data: &[u8], path: &[&str]) -> Found {
        let mut level = top_level(data);
        let mut result = None;

        for (depth, wanted) in path.iter().enumerate() {
            let found = level
                .into_iter()
                .find(|b| b.kind == *wanted)
                .unwrap_or_else(|| panic!("boîte « {wanted} » absente au niveau {depth}"));

            level = if depth + 1 < path.len() {
                boxes_in(data, found.start + children_offset(&found.kind), found.end)
            } else {
                Vec::new()
            };

            result = Some(found);
        }

        result.expect("chemin non vide")
    }

    /// Écrit un fichier de test et rend son contenu.
    fn write_file(frames: &[(u64, bool, usize)]) -> Vec<u8> {
        let dir = tempfile::tempdir().expect("dossier temporaire");
        let path = dir.path().join("test.mp4");

        let mut writer =
            Fmp4Writer::create(&path, "test.mp4".to_string(), 640, 480, 25, &SPS, &PPS)
                .expect("création");

        for &(timestamp, keyframe, payload) in frames {
            let mut nals = Vec::new();

            if keyframe {
                // Une image clé réelle porte toujours ses paramètres.
                nals.push(SPS.to_vec());
                nals.push(PPS.to_vec());
            }

            let mut slice = vec![if keyframe { 0x65u8 } else { 0x41 }];
            slice.extend(std::iter::repeat_n(7u8, payload));
            nals.push(slice);

            writer
                .write_frame(&nals, timestamp, keyframe)
                .expect("écriture");
        }

        writer.finish().expect("fermeture");
        std::fs::read(&path).expect("relecture")
    }

    // --- Segment d'initialisation ---

    #[test]
    fn the_file_opens_with_an_initialisation_segment() {
        // `ftyp` puis `moov` AVANT la moindre image : c'est ce qui rend un
        // fichier tronqué malgré tout lisible.
        let data = write_file(&[(0, true, 100)]);
        let top: Vec<String> = top_level(&data).into_iter().map(|b| b.kind).collect();

        assert_eq!(top[0], "ftyp");
        assert_eq!(top[1], "moov");
    }

    #[test]
    fn every_box_declares_a_size_consistent_with_its_parent() {
        // `boxes_in` vérifie à chaque niveau qu'aucune boîte ne déborde et
        // qu'aucun octet ne traîne : parcourir tout l'arbre suffit à prouver
        // que les tailles sont cohérentes de bout en bout.
        let data = write_file(&[(0, true, 200), (3600, false, 40), (7200, false, 40)]);

        let moov = find(&data, &["moov"]);
        boxes_in(&data, moov.start + 8, moov.end);

        let stbl = find(&data, &["moov", "trak", "mdia", "minf", "stbl"]);
        boxes_in(&data, stbl.start + 8, stbl.end);
    }

    #[test]
    fn the_track_declares_the_fragments_that_follow() {
        // Sans `mvex`, un lecteur s'en tient au `moov` — qui ne décrit aucune
        // image — et conclut que le fichier est vide.
        let data = write_file(&[(0, true, 100)]);
        find(&data, &["moov", "mvex", "trex"]);
    }

    #[test]
    fn the_decoder_configuration_carries_the_parameter_sets() {
        let data = write_file(&[(0, true, 100)]);
        let avcc = find(
            &data,
            &[
                "moov", "trak", "mdia", "minf", "stbl", "stsd", "avc1", "avcC",
            ],
        );

        let body = &data[avcc.start + 8..avcc.end];

        assert_eq!(body[0], 1, "version de configuration");
        assert_eq!(&body[1..4], &SPS[1..4], "profil, compatibilité, niveau");
        assert_eq!(body[4] & 0x03, 3, "longueurs de NAL sur 4 octets");
        assert_eq!(body[5] & 0x1F, 1, "un SPS");

        let sps_len = u16::from_be_bytes([body[6], body[7]]) as usize;
        assert_eq!(&body[8..8 + sps_len], &SPS);

        let after_sps = 8 + sps_len;
        assert_eq!(body[after_sps], 1, "un PPS");
        let pps_len = u16::from_be_bytes([body[after_sps + 1], body[after_sps + 2]]) as usize;
        assert_eq!(&body[after_sps + 3..after_sps + 3 + pps_len], &PPS);
    }

    #[test]
    fn the_track_declares_the_real_dimensions() {
        let data = write_file(&[(0, true, 100)]);
        let avc1 = find(
            &data,
            &["moov", "trak", "mdia", "minf", "stbl", "stsd", "avc1"],
        );

        // Dans `avc1`, les dimensions suivent 24 octets de champs réservés.
        let body = &data[avc1.start + 8..];
        assert_eq!(u16::from_be_bytes([body[24], body[25]]), 640);
        assert_eq!(u16::from_be_bytes([body[26], body[27]]), 480);
    }

    // --- Fragments ---

    #[test]
    fn a_fragment_is_written_per_group_of_pictures() {
        // Un fragment commence à une image clé : c'est le seul endroit où un
        // lecteur peut entrer sans dépendre de ce qui précède.
        let data = write_file(&[
            (0, true, 100),
            (3600, false, 40),
            (7200, true, 100),
            (10800, false, 40),
        ]);

        let kinds: Vec<String> = top_level(&data).into_iter().map(|b| b.kind).collect();

        assert_eq!(
            kinds,
            vec!["ftyp", "moov", "moof", "mdat", "moof", "mdat"],
            "deux images clés doivent donner deux fragments"
        );
    }

    #[test]
    fn a_single_group_of_pictures_is_written_once_on_close() {
        // La dernière image clé n'est suivie d'aucune autre : c'est
        // `finish` qui doit écrire son fragment, sans quoi la fin de
        // l'enregistrement serait perdue.
        let data = write_file(&[(0, true, 100), (3600, false, 40)]);
        let kinds: Vec<String> = top_level(&data).into_iter().map(|b| b.kind).collect();

        assert_eq!(kinds, vec!["ftyp", "moov", "moof", "mdat"]);
    }

    #[test]
    fn the_data_offset_points_at_the_start_of_the_media_data() {
        // LE calcul le plus facile à rater du format : compté depuis le début
        // du `moof`, il doit tomber exactement sur le premier octet de
        // données du `mdat`. Un décalage de quelques octets et le lecteur
        // décode du bruit.
        let data = write_file(&[(0, true, 100), (3600, false, 40)]);

        let moof = find(&data, &["moof"]);
        let trun = find(&data, &["moof", "traf", "trun"]);

        // `trun` : version/drapeaux (4), nombre d'échantillons (4), puis le
        // décalage.
        let offset_position = trun.start + 8 + 4 + 4;
        let data_offset = u32::from_be_bytes(
            data[offset_position..offset_position + 4]
                .try_into()
                .unwrap(),
        ) as usize;

        let mdat = top_level(&data)
            .into_iter()
            .find(|b| b.kind == "mdat")
            .expect("mdat");

        assert_eq!(
            moof.start + data_offset,
            mdat.start + 8,
            "le décalage doit viser le premier octet de données du mdat"
        );
    }

    #[test]
    fn the_sample_sizes_add_up_to_the_media_data() {
        // Si les tailles déclarées ne correspondaient pas aux données
        // écrites, le lecteur décalerait progressivement et finirait sur du
        // bruit — sans qu'aucune boîte ne soit malformée pour autant.
        let data = write_file(&[(0, true, 300), (3600, false, 50), (7200, false, 70)]);

        let trun = find(&data, &["moof", "traf", "trun"]);
        let body = &data[trun.start + 8..trun.end];

        let count = u32::from_be_bytes(body[4..8].try_into().unwrap()) as usize;
        assert_eq!(count, 3);

        // Après version/drapeaux (4), nombre (4) et décalage (4) : douze
        // octets par échantillon (durée, taille, propriétés).
        let declared: u32 = (0..count)
            .map(|index| {
                let at = 12 + index * 12 + 4;
                u32::from_be_bytes(body[at..at + 4].try_into().unwrap())
            })
            .sum();

        let mdat = top_level(&data)
            .into_iter()
            .find(|b| b.kind == "mdat")
            .expect("mdat");

        assert_eq!(declared as usize, mdat.end - (mdat.start + 8));
    }

    #[test]
    fn sample_durations_follow_the_real_timestamps() {
        // La cadence d'une caméra n'est pas constante : c'est précisément ce
        // que le format d'enregistrement précédent corrigeait déjà, et qu'on
        // ne doit pas reperdre en passant au MP4.
        let data = write_file(&[(0, true, 100), (3600, false, 40), (12600, false, 40)]);

        let trun = find(&data, &["moof", "traf", "trun"]);
        let body = &data[trun.start + 8..trun.end];

        let duration = |index: usize| {
            let at = 12 + index * 12;
            u32::from_be_bytes(body[at..at + 4].try_into().unwrap())
        };

        assert_eq!(duration(0), 3_600, "écart réel jusqu'à la deuxième image");
        assert_eq!(duration(1), 9_000, "écart réel jusqu'à la troisième");
        // La dernière n'a pas de suivante : elle garde la durée nominale.
        assert_eq!(duration(2), TIMESCALE / 25);
    }

    #[test]
    fn keyframes_are_flagged_so_players_can_seek_to_them() {
        let data = write_file(&[(0, true, 100), (3600, false, 40)]);

        let trun = find(&data, &["moof", "traf", "trun"]);
        let body = &data[trun.start + 8..trun.end];

        let flags = |index: usize| {
            let at = 12 + index * 12 + 8;
            u32::from_be_bytes(body[at..at + 4].try_into().unwrap())
        };

        assert_eq!(flags(0), SAMPLE_FLAGS_KEYFRAME);
        assert_eq!(flags(1), SAMPLE_FLAGS_DELTA);
    }

    #[test]
    fn each_fragment_declares_its_start_on_the_media_timeline() {
        // `tfdt` : sans lui, un lecteur place tous les fragments à zéro et la
        // lecture bégaie sur place.
        let data = write_file(&[(0, true, 100), (3600, false, 40), (90_000, true, 100)]);

        let second_moof = top_level(&data)
            .into_iter()
            .filter(|b| b.kind == "moof")
            .nth(1)
            .expect("deuxième fragment");

        let tfdt = boxes_in(&data, second_moof.start + 8, second_moof.end)
            .into_iter()
            .find(|b| b.kind == "traf")
            .map(|traf| {
                boxes_in(&data, traf.start + 8, traf.end)
                    .into_iter()
                    .find(|b| b.kind == "tfdt")
                    .expect("tfdt")
            })
            .expect("traf");

        // Version 1 : horodatage sur 64 bits, après version et drapeaux.
        let at = tfdt.start + 12;
        assert_eq!(
            u64::from_be_bytes(data[at..at + 8].try_into().unwrap()),
            90_000
        );
    }

    #[test]
    fn fragment_sequence_numbers_increase() {
        let data = write_file(&[(0, true, 100), (90_000, true, 100), (180_000, true, 100)]);

        let sequences: Vec<u32> = top_level(&data)
            .into_iter()
            .filter(|b| b.kind == "moof")
            .map(|moof| {
                let mfhd = boxes_in(&data, moof.start + 8, moof.end)
                    .into_iter()
                    .find(|b| b.kind == "mfhd")
                    .expect("mfhd");
                u32::from_be_bytes(data[mfhd.start + 12..mfhd.start + 16].try_into().unwrap())
            })
            .collect();

        assert_eq!(sequences, vec![1, 2, 3]);
    }

    // --- Échantillons ---

    #[test]
    fn samples_carry_length_prefixed_nals() {
        let data = write_file(&[(0, true, 20)]);

        let mdat = top_level(&data)
            .into_iter()
            .find(|b| b.kind == "mdat")
            .expect("mdat");
        let payload = &data[mdat.start + 8..mdat.end];

        // Une seule NAL conservée (la tranche) : sa longueur, puis elle.
        let length = u32::from_be_bytes(payload[0..4].try_into().unwrap()) as usize;
        assert_eq!(length, 21, "en-tête de NAL + 20 octets");
        assert_eq!(payload[4], 0x65, "tranche d'image clé");
        assert_eq!(payload.len(), 4 + length);
    }

    #[test]
    fn parameter_sets_are_not_repeated_in_every_keyframe() {
        // Ils sont déjà dans `avcC`. Les répéter dans chaque image clé ne
        // ferait que grossir le fichier, et c'est l'identifiant constant des
        // jeux de paramètres qui rend l'économie sûre.
        let data = write_file(&[(0, true, 20)]);

        let mdat = top_level(&data)
            .into_iter()
            .find(|b| b.kind == "mdat")
            .expect("mdat");
        let payload = &data[mdat.start + 8..mdat.end];

        assert!(
            !payload.windows(SPS.len()).any(|w| w == SPS),
            "le SPS ne doit pas figurer dans les données"
        );
    }

    #[test]
    fn an_existing_file_is_never_overwritten() {
        // Même garde que les enregistrements : `create_new` plutôt que
        // `create`, qui tronquerait sans un mot.
        let dir = tempfile::tempdir().expect("dossier temporaire");
        let path = dir.path().join("occupe.mp4");
        std::fs::write(&path, b"occupe").expect("écriture");

        let result = Fmp4Writer::create(&path, "occupe.mp4".to_string(), 64, 64, 25, &SPS, &PPS);

        assert!(result.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"occupe");
    }

    #[test]
    fn the_total_duration_is_written_on_close() {
        // Sans elle, chaque lecteur doit la deviner en parcourant les
        // fragments : la durée affichée reste approximative, et la barre de
        // progression saute pendant toute la lecture.
        let data = write_file(&[(0, true, 100), (3600, false, 40), (7200, false, 40)]);

        let mvhd = find(&data, &["moov", "mvhd"]);
        // Version 1 : après version/drapeaux (4), dates (16) et échelle (4).
        let at = mvhd.start + 8 + 4 + 16 + 4;
        let movie_duration = u64::from_be_bytes(data[at..at + 8].try_into().unwrap());

        // Deux écarts réels de 3600, puis la durée nominale de la dernière.
        assert_eq!(movie_duration, 3_600 + 3_600 + u64::from(TIMESCALE / 25));
    }

    #[test]
    fn the_three_duration_fields_agree() {
        // Film, piste et média partagent la même base de temps ici : des
        // valeurs divergentes feraient afficher trois durées différentes
        // selon le lecteur.
        let data = write_file(&[(0, true, 100), (3600, false, 40)]);

        let mvhd = find(&data, &["moov", "mvhd"]);
        let movie = u64::from_be_bytes(data[mvhd.start + 32..mvhd.start + 40].try_into().unwrap());

        let tkhd = find(&data, &["moov", "trak", "tkhd"]);
        // Version 1 : version/drapeaux (4), dates (16), identifiant (4),
        // réservé (4).
        let at = tkhd.start + 8 + 4 + 16 + 4 + 4;
        let track = u64::from_be_bytes(data[at..at + 8].try_into().unwrap());

        let mdhd = find(&data, &["moov", "trak", "mdia", "mdhd"]);
        let at = mdhd.start + 8 + 4 + 16 + 4;
        let media = u64::from_be_bytes(data[at..at + 8].try_into().unwrap());

        assert_eq!(movie, track);
        assert_eq!(movie, media);
        assert!(movie > 0);
    }

    #[test]
    fn a_recording_without_any_frame_is_still_a_valid_file() {
        // Enregistrement arrêté avant la première image : le fichier doit
        // rester analysable, même s'il ne montre rien.
        let data = write_file(&[]);
        let kinds: Vec<String> = top_level(&data).into_iter().map(|b| b.kind).collect();

        assert_eq!(kinds, vec!["ftyp", "moov"]);
    }
}
