//! Empaquetage RTP du flux H.264 (RFC 3550 pour RTP, RFC 6184 pour la
//! charge utile H.264) et rapports RTCP (RFC 3550 §6.4).
//!
//! # Pourquoi écrire ça à la main
//!
//! RTP est un en-tête de 12 octets suivi de la charge utile ; la seule
//! subtilité est le découpage des NAL trop grosses pour un datagramme. Le
//! faire ici évite d'ajouter au Raspberry Pi une pile média complète (et son
//! lot de dépendances natives) pour trois structures de bits.

use std::time::{SystemTime, UNIX_EPOCH};

/// Type de charge utile DYNAMIQUE attribué au H.264.
///
/// H.264 n'a pas de type statique dans la table RFC 3551 : il faut en choisir
/// un dans la plage dynamique (96-127) et l'annoncer dans le SDP (voir
/// [`super::sdp`]). 96 est la valeur conventionnelle, celle qu'attendent les
/// lecteurs par habitude.
pub const H264_PAYLOAD_TYPE: u8 = 96;

/// Horloge RTP du H.264, imposée par RFC 6184 §8.1.
pub const H264_CLOCK_RATE: u32 = 90_000;

/// Taille maximale de la charge utile d'un paquet RTP.
///
/// Calibrée pour qu'un paquet complet (20 octets d'IPv4 + 8 d'UDP + 12 de
/// RTP + charge utile) tienne sous les 1500 octets d'une trame Ethernet
/// standard : au-delà, l'IP fragmente, et la perte d'UN fragment détruit tout
/// le paquet RTP. La marge couvre un éventuel encapsulage (VLAN, VPN).
pub const MAX_PAYLOAD: usize = 1400;

/// Type de NAL « FU-A » : NAL fragmentée sur plusieurs paquets RTP
/// (RFC 6184 §5.8).
const NAL_TYPE_FU_A: u8 = 28;

/// Empaqueteur RTP d'un flux : porte le numéro de séquence et les compteurs
/// nécessaires aux rapports RTCP.
///
/// Un empaqueteur PAR CLIENT : le numéro de séquence est propre à une
/// session RTP, deux clients ne peuvent pas le partager.
pub struct Packetizer {
    ssrc: u32,
    sequence: u16,
    /// Nombre de paquets RTP émis, pour le rapport RTCP.
    packets_sent: u32,
    /// Nombre d'octets de CHARGE UTILE émis (en-têtes RTP exclus), comme
    /// l'exige RFC 3550 §6.4.1.
    octets_sent: u32,
    /// Dernier horodatage RTP émis, repris dans le rapport RTCP pour que le
    /// client puisse corréler horloge média et horloge murale.
    last_timestamp: u32,
}

impl Packetizer {
    /// Crée un empaqueteur avec un SSRC (identifiant de source) dérivé de
    /// l'heure et du PID.
    ///
    /// Le SSRC doit seulement être unique au sein d'une session : deux
    /// clients du même flux ne doivent pas se voir attribuer le même, sinon
    /// un routeur ou un mélangeur sur le chemin les confondrait.
    pub fn new(salt: u32) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);

        Self {
            ssrc: now ^ (std::process::id().rotate_left(16)) ^ salt.wrapping_mul(2_654_435_761),
            // Un numéro de séquence initial non nul est recommandé
            // (RFC 3550 §5.1) : il ne doit pas être devinable.
            sequence: (now & 0xFFFF) as u16,
            packets_sent: 0,
            octets_sent: 0,
            last_timestamp: 0,
        }
    }

    pub fn ssrc(&self) -> u32 {
        self.ssrc
    }

    /// Vrai dès qu'au moins un paquet RTP a été émis.
    ///
    /// Sert à ne pas envoyer de rapport RTCP avant la première frame : il ne
    /// contiendrait que des zéros et un horodatage média qui ne correspond à
    /// rien, ce qui est au mieux inutile et au pire trompeur pour le calcul
    /// de gigue du lecteur.
    pub fn has_sent_packets(&self) -> bool {
        self.packets_sent > 0
    }

    /// Découpe une unité d'accès H.264 en paquets RTP prêts à être envoyés.
    ///
    /// Chaque NAL part soit en UN paquet (« single NAL unit mode »,
    /// RFC 6184 §5.6), soit en plusieurs paquets FU-A si elle dépasse
    /// [`MAX_PAYLOAD`] — c'est systématiquement le cas des images clés, qui
    /// pèsent plusieurs dizaines de kilo-octets.
    ///
    /// Le bit *marker* est posé sur le DERNIER paquet de l'unité d'accès :
    /// c'est ce qui indique au décodeur que la frame est complète et qu'il
    /// peut l'afficher. Sans lui, le lecteur attend indéfiniment la suite et
    /// accumule une latence d'une frame.
    pub fn packetize(&mut self, nals: &[Vec<u8>], timestamp: u32) -> Vec<Vec<u8>> {
        self.last_timestamp = timestamp;

        // Les NAL vides sont écartées d'abord : c'est la dernière NAL NON
        // VIDE qui doit porter le bit marker.
        let nals: Vec<&Vec<u8>> = nals.iter().filter(|nal| !nal.is_empty()).collect();

        let mut packets = Vec::new();

        for (index, nal) in nals.iter().enumerate() {
            let is_last_nal = index + 1 == nals.len();

            if nal.len() <= MAX_PAYLOAD {
                packets.push(self.single_nal_packet(nal, timestamp, is_last_nal));
            } else {
                self.fragment_nal(nal, timestamp, is_last_nal, &mut packets);
            }
        }

        packets
    }

    /// Un paquet RTP contenant une NAL entière (RFC 6184 §5.6).
    fn single_nal_packet(&mut self, nal: &[u8], timestamp: u32, marker: bool) -> Vec<u8> {
        let mut packet = Vec::with_capacity(12 + nal.len());
        self.write_header(&mut packet, timestamp, marker);
        packet.extend_from_slice(nal);

        self.packets_sent = self.packets_sent.wrapping_add(1);
        self.octets_sent = self.octets_sent.wrapping_add(nal.len() as u32);

        packet
    }

    /// Découpe une NAL en paquets FU-A (RFC 6184 §5.8).
    ///
    /// L'octet d'en-tête de la NAL d'origine est REMPLACÉ par deux octets :
    /// un en-tête FU indicator qui reprend ses bits d'importance (`NRI`) avec
    /// le type 28, et un en-tête FU header qui porte le type réel de la NAL
    /// plus deux drapeaux de début et de fin de fragment. Le décodeur
    /// reconstitue la NAL en recollant les charges utiles.
    fn fragment_nal(
        &mut self,
        nal: &[u8],
        timestamp: u32,
        is_last_nal: bool,
        packets: &mut Vec<Vec<u8>>,
    ) {
        let header = nal[0];
        let indicator = (header & 0xE0) | NAL_TYPE_FU_A;
        let nal_type = header & 0x1F;

        // Deux octets d'en-tête FU mangent sur la charge utile disponible.
        let chunk_size = MAX_PAYLOAD - 2;
        // Le premier octet (l'en-tête de la NAL) n'est pas transmis : il est
        // reconstruit par le décodeur à partir des en-têtes FU.
        let body = &nal[1..];
        let chunks: Vec<&[u8]> = body.chunks(chunk_size).collect();

        for (index, chunk) in chunks.iter().enumerate() {
            let is_first_fragment = index == 0;
            let is_last_fragment = index + 1 == chunks.len();

            let mut fu_header = nal_type;
            if is_first_fragment {
                fu_header |= 0x80; // bit S (start)
            }
            if is_last_fragment {
                fu_header |= 0x40; // bit E (end)
            }

            let mut packet = Vec::with_capacity(14 + chunk.len());
            // Marker seulement sur le tout dernier paquet de l'unité
            // d'accès : une frame fragmentée n'est complète qu'à son dernier
            // fragment.
            self.write_header(&mut packet, timestamp, is_last_nal && is_last_fragment);
            packet.push(indicator);
            packet.push(fu_header);
            packet.extend_from_slice(chunk);

            self.packets_sent = self.packets_sent.wrapping_add(1);
            self.octets_sent = self.octets_sent.wrapping_add(2 + chunk.len() as u32);

            packets.push(packet);
        }
    }

    /// Écrit l'en-tête RTP de 12 octets (RFC 3550 §5.1).
    fn write_header(&mut self, packet: &mut Vec<u8>, timestamp: u32, marker: bool) {
        // Octet 0 : version 2 (bits 7-6), pas de padding, pas d'extension,
        // aucun CSRC.
        packet.push(0x80);
        // Octet 1 : bit marker puis les 7 bits du type de charge utile.
        packet.push(if marker {
            0x80 | H264_PAYLOAD_TYPE
        } else {
            H264_PAYLOAD_TYPE
        });
        packet.extend_from_slice(&self.sequence.to_be_bytes());
        packet.extend_from_slice(&timestamp.to_be_bytes());
        packet.extend_from_slice(&self.ssrc.to_be_bytes());

        // `wrapping_add` : le numéro de séquence est explicitement un
        // compteur sur 16 bits qui boucle (RFC 3550 §5.1), le client gère le
        // repli.
        self.sequence = self.sequence.wrapping_add(1);
    }

    /// Construit un rapport d'émetteur RTCP (« Sender Report », RFC 3550
    /// §6.4.1).
    ///
    /// Il associe l'horodatage média (horloge 90 kHz) à l'heure murale NTP :
    /// c'est ce qui permet au lecteur de connaître la cadence réelle du flux
    /// et d'en déduire son tampon de gigue. Sans rapport, certains lecteurs
    /// (dont VLC) finissent par supposer que la source est morte.
    pub fn sender_report(&self) -> Vec<u8> {
        let mut packet = Vec::with_capacity(28);

        packet.push(0x80); // version 2, pas de padding, 0 bloc de réception
        packet.push(200); // SR
        // Longueur en mots de 32 bits, moins un (RFC 3550 §6.4.1).
        packet.extend_from_slice(&6u16.to_be_bytes());
        packet.extend_from_slice(&self.ssrc.to_be_bytes());

        let (ntp_seconds, ntp_fraction) = ntp_now();
        packet.extend_from_slice(&ntp_seconds.to_be_bytes());
        packet.extend_from_slice(&ntp_fraction.to_be_bytes());
        packet.extend_from_slice(&self.last_timestamp.to_be_bytes());
        packet.extend_from_slice(&self.packets_sent.to_be_bytes());
        packet.extend_from_slice(&self.octets_sent.to_be_bytes());

        packet
    }
}

/// Heure courante au format NTP : secondes depuis 1900 et fraction de
/// seconde sur 32 bits.
///
/// L'écart entre l'époque NTP (1900) et l'époque Unix (1970) est de 70 ans,
/// dont 17 années bissextiles.
fn ntp_now() -> (u32, u32) {
    const UNIX_TO_NTP_SECONDS: u64 = 2_208_988_800;

    let Ok(since_epoch) = SystemTime::now().duration_since(UNIX_EPOCH) else {
        return (0, 0);
    };

    let seconds = (since_epoch.as_secs() + UNIX_TO_NTP_SECONDS) as u32;
    let fraction = ((u64::from(since_epoch.subsec_nanos()) << 32) / 1_000_000_000) as u32;

    (seconds, fraction)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lit les champs de l'en-tête RTP d'un paquet : (marker, séquence,
    /// horodatage, ssrc).
    fn header_of(packet: &[u8]) -> (bool, u16, u32, u32) {
        (
            packet[1] & 0x80 != 0,
            u16::from_be_bytes([packet[2], packet[3]]),
            u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]),
            u32::from_be_bytes([packet[8], packet[9], packet[10], packet[11]]),
        )
    }

    #[test]
    fn a_small_nal_travels_in_a_single_packet_carrying_it_verbatim() {
        let mut packetizer = Packetizer::new(1);
        let nal = vec![0x65u8, 1, 2, 3, 4];

        let packets = packetizer.packetize(std::slice::from_ref(&nal), 9000);

        assert_eq!(packets.len(), 1);
        assert_eq!(&packets[0][12..], nal.as_slice());
    }

    #[test]
    fn the_rtp_header_announces_version_two_and_the_h264_payload_type() {
        let mut packetizer = Packetizer::new(1);
        let packets = packetizer.packetize(&[vec![0x65, 1, 2]], 9000);

        // Version 2 dans les deux bits de poids fort.
        assert_eq!(packets[0][0] >> 6, 2);
        assert_eq!(packets[0][1] & 0x7F, H264_PAYLOAD_TYPE);
    }

    #[test]
    fn the_timestamp_and_ssrc_are_written_big_endian() {
        // Un en-tête RTP est en ordre réseau : une inversion donnerait un
        // flux que personne ne sait lire.
        let mut packetizer = Packetizer::new(1);
        let ssrc = packetizer.ssrc();

        let packets = packetizer.packetize(&[vec![0x65, 1]], 0x0102_0304);
        let (_, _, timestamp, written_ssrc) = header_of(&packets[0]);

        assert_eq!(timestamp, 0x0102_0304);
        assert_eq!(written_ssrc, ssrc);
    }

    #[test]
    fn sequence_numbers_increment_by_one_per_packet() {
        let mut packetizer = Packetizer::new(1);

        let first = packetizer.packetize(&[vec![0x65, 1]], 0);
        let second = packetizer.packetize(&[vec![0x41, 1]], 3600);

        let (_, first_sequence, _, _) = header_of(&first[0]);
        let (_, second_sequence, _, _) = header_of(&second[0]);

        assert_eq!(second_sequence.wrapping_sub(first_sequence), 1);
    }

    #[test]
    fn only_the_last_packet_of_an_access_unit_is_marked() {
        // Le bit marker dit au décodeur « la frame est complète ». Posé trop
        // tôt, il affiche une image incomplète ; oublié, il attend la suite.
        let mut packetizer = Packetizer::new(1);

        let packets =
            packetizer.packetize(&[vec![0x67, 1, 2], vec![0x68, 3], vec![0x65, 4, 5]], 9000);

        assert_eq!(packets.len(), 3);
        assert!(!header_of(&packets[0]).0);
        assert!(!header_of(&packets[1]).0);
        assert!(header_of(&packets[2]).0);
    }

    #[test]
    fn a_large_nal_is_fragmented_into_fu_a_packets() {
        let mut packetizer = Packetizer::new(1);
        // Une image clé réaliste : bien au-delà d'un datagramme.
        let mut nal = vec![0x65u8];
        nal.extend((0..5000).map(|i| (i % 251) as u8));

        let packets = packetizer.packetize(&[nal.clone()], 9000);

        assert!(packets.len() > 1, "{} paquet(s)", packets.len());

        for packet in &packets {
            assert!(packet.len() <= 12 + MAX_PAYLOAD);
            // FU indicator : type 28, bits NRI de la NAL d'origine préservés.
            assert_eq!(packet[12] & 0x1F, NAL_TYPE_FU_A);
            assert_eq!(packet[12] & 0xE0, nal[0] & 0xE0);
            // FU header : le type réel de la NAL d'origine.
            assert_eq!(packet[13] & 0x1F, nal[0] & 0x1F);
        }
    }

    #[test]
    fn the_fragments_carry_start_and_end_flags_exactly_once() {
        let mut packetizer = Packetizer::new(1);
        let mut nal = vec![0x65u8];
        nal.extend(std::iter::repeat_n(7u8, 5000));

        let packets = packetizer.packetize(&[nal], 9000);

        let starts = packets.iter().filter(|p| p[13] & 0x80 != 0).count();
        let ends = packets.iter().filter(|p| p[13] & 0x40 != 0).count();

        assert_eq!(starts, 1);
        assert_eq!(ends, 1);
        assert!(
            packets[0][13] & 0x80 != 0,
            "le bit S doit être sur le premier"
        );
        assert!(
            packets[packets.len() - 1][13] & 0x40 != 0,
            "le bit E doit être sur le dernier"
        );
    }

    #[test]
    fn reassembling_the_fragments_restores_the_original_nal() {
        // Le test qui compte vraiment : ce qu'un décodeur reconstruirait doit
        // être exactement la NAL de départ.
        let mut packetizer = Packetizer::new(1);
        let mut nal = vec![0x65u8];
        nal.extend((0..5000).map(|i| (i % 251) as u8));

        let packets = packetizer.packetize(&[nal.clone()], 9000);

        let mut reassembled = vec![nal[0]];
        for packet in &packets {
            reassembled.extend_from_slice(&packet[14..]);
        }

        assert_eq!(reassembled, nal);
    }

    #[test]
    fn empty_nals_are_dropped_without_stealing_the_marker_bit() {
        // Une NAL vide en fin de liste ne doit pas emporter le bit marker,
        // sinon la frame n'est jamais annoncée comme complète.
        let mut packetizer = Packetizer::new(1);

        let packets = packetizer.packetize(&[vec![0x65, 1, 2], Vec::new()], 9000);

        assert_eq!(packets.len(), 1);
        assert!(header_of(&packets[0]).0);
    }

    #[test]
    fn a_sender_report_has_the_rtcp_shape_expected_by_players() {
        let mut packetizer = Packetizer::new(1);
        packetizer.packetize(&[vec![0x65, 1, 2, 3]], 9000);

        let report = packetizer.sender_report();

        assert_eq!(report.len(), 28);
        assert_eq!(report[0] >> 6, 2, "version RTP");
        assert_eq!(report[1], 200, "type de paquet SR");
        assert_eq!(u16::from_be_bytes([report[2], report[3]]), 6, "longueur");
        assert_eq!(
            u32::from_be_bytes([report[4], report[5], report[6], report[7]]),
            packetizer.ssrc()
        );
        // L'horodatage RTP rapporté est celui de la dernière frame émise.
        assert_eq!(
            u32::from_be_bytes([report[16], report[17], report[18], report[19]]),
            9000
        );
        assert_eq!(
            u32::from_be_bytes([report[20], report[21], report[22], report[23]]),
            1,
            "un paquet émis"
        );
    }

    #[test]
    fn the_sender_report_counts_payload_octets_not_packet_octets() {
        // RFC 3550 §6.4.1 : le compteur exclut les en-têtes RTP. S'il les
        // incluait, le client surestimerait le débit de 12 octets par paquet.
        let mut packetizer = Packetizer::new(1);
        packetizer.packetize(&[vec![0x65, 1, 2, 3]], 9000);

        let report = packetizer.sender_report();
        let octets = u32::from_be_bytes([
            report[20 + 4],
            report[21 + 4],
            report[22 + 4],
            report[23 + 4],
        ]);

        assert_eq!(octets, 4);
    }

    #[test]
    fn nothing_is_reported_before_the_first_packet() {
        let mut packetizer = Packetizer::new(1);
        assert!(!packetizer.has_sent_packets());

        packetizer.packetize(&[vec![0x65, 1]], 0);
        assert!(packetizer.has_sent_packets());
    }

    #[test]
    fn two_packetizers_do_not_share_an_ssrc() {
        // Deux lecteurs sur le même flux sont deux sources RTP distinctes.
        assert_ne!(Packetizer::new(1).ssrc(), Packetizer::new(2).ssrc());
    }

    #[test]
    fn the_ntp_timestamp_is_after_the_ntp_epoch_by_more_than_a_century() {
        // Garde-fou sur la constante d'époque : une erreur de 70 ans donne un
        // rapport que les lecteurs jugent aberrant.
        let (seconds, _) = ntp_now();
        // 2026 - 1900 = 126 ans, soit ~3,97 milliards de secondes.
        assert!(seconds > 3_900_000_000, "{seconds}");
    }
}
