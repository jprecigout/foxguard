//! Négociation du transport RTP (en-tête `Transport` d'un `SETUP`,
//! RFC 2326 §12.39).
//!
//! Un lecteur annonce par quel canal il veut recevoir le flux. Deux formes
//! nous intéressent :
//!
//! - **`RTP/AVP/TCP;interleaved=0-1`** : les paquets RTP voyagent *dans* la
//!   connexion RTSP déjà établie, préfixés d'un petit en-tête de
//!   multiplexage. Une seule connexion, sortante côté client : ça traverse
//!   tous les pare-feux et toutes les NAT. C'est le transport à privilégier
//!   sur un réseau domestique, et celui que les lecteurs choisissent en
//!   repli ;
//! - **`RTP/AVP;client_port=5000-5001`** : RTP en UDP vers deux ports que le
//!   client a ouverts. Moins de surcoût, pas de retransmission TCP qui
//!   accumule du retard — mais il faut que le serveur puisse réellement
//!   joindre ces ports.

/// Transport négocié pour une session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// RTP entrelacé dans la connexion RTSP, sur deux canaux consécutifs.
    Interleaved { rtp_channel: u8, rtcp_channel: u8 },
    /// RTP en UDP vers les ports que le client a annoncés.
    Udp { rtp_port: u16, rtcp_port: u16 },
}

impl Transport {
    /// Choisit un transport parmi ceux qu'un client propose.
    ///
    /// L'en-tête peut contenir PLUSIEURS propositions séparées par des
    /// virgules, par ordre de préférence décroissante du client (RFC 2326
    /// §12.39) : on retient la première qu'on sait servir, plutôt que
    /// d'imposer la nôtre.
    pub fn negotiate(header: &str) -> Option<Self> {
        header.split(',').find_map(Self::parse_one)
    }

    /// Analyse UNE proposition de transport.
    fn parse_one(specification: &str) -> Option<Self> {
        let parameters: Vec<&str> = specification.split(';').map(str::trim).collect();

        let protocol = parameters.first()?.to_ascii_uppercase();

        // Le profil doit être RTP/AVP : ni SRTP (chiffré, non géré), ni
        // RTP/AVPF (retour d'information immédiat, non géré).
        if !protocol.starts_with("RTP/AVP") {
            return None;
        }

        // `multicast` : ce serveur n'émet qu'en unicast. Mieux vaut refuser
        // la proposition — le client en fera une autre — que d'accepter puis
        // d'envoyer dans le vide.
        if parameters
            .iter()
            .any(|p| p.eq_ignore_ascii_case("multicast"))
        {
            return None;
        }

        let is_tcp = protocol.ends_with("/TCP");

        if is_tcp {
            // `interleaved` est en principe obligatoire ici, mais certains
            // clients l'omettent en comptant sur les canaux 0 et 1.
            let (rtp_channel, rtcp_channel) = parameters
                .iter()
                .find_map(|p| parse_pair(p, "interleaved"))
                .map(|(rtp, rtcp)| (rtp as u8, rtcp as u8))
                .unwrap_or((0, 1));

            return Some(Self::Interleaved {
                rtp_channel,
                rtcp_channel,
            });
        }

        // En UDP, les ports du client sont indispensables : sans eux, il n'y
        // a nulle part où envoyer le flux.
        let (rtp_port, rtcp_port) = parameters
            .iter()
            .find_map(|p| parse_pair(p, "client_port"))?;

        if rtp_port == 0 {
            return None;
        }

        Some(Self::Udp {
            rtp_port,
            rtcp_port,
        })
    }

    /// Valeur de l'en-tête `Transport` à renvoyer dans la réponse au
    /// `SETUP`.
    ///
    /// Le client y lit la confirmation de ce qui a été retenu. `ssrc` lui
    /// permet de reconnaître notre flux parmi d'autres sources sur le même
    /// port.
    pub fn response_header(&self, ssrc: u32, server_ports: Option<(u16, u16)>) -> String {
        match self {
            Self::Interleaved {
                rtp_channel,
                rtcp_channel,
            } => format!(
                "RTP/AVP/TCP;unicast;interleaved={rtp_channel}-{rtcp_channel};ssrc={ssrc:08X}"
            ),
            Self::Udp {
                rtp_port,
                rtcp_port,
            } => {
                let mut header = format!("RTP/AVP;unicast;client_port={rtp_port}-{rtcp_port}");

                // Les ports d'émission du serveur, que le client utilise pour
                // lui adresser ses rapports RTCP.
                if let Some((rtp, rtcp)) = server_ports {
                    header.push_str(&format!(";server_port={rtp}-{rtcp}"));
                }

                header.push_str(&format!(";ssrc={ssrc:08X}"));
                header
            }
        }
    }
}

/// Analyse un paramètre de la forme `nom=début-fin`.
///
/// Le second terme est optionnel : certains clients n'annoncent qu'un port,
/// auquel cas RFC 2326 veut que le suivant soit utilisé pour RTCP.
fn parse_pair(parameter: &str, name: &str) -> Option<(u16, u16)> {
    let value = parameter.strip_prefix(name)?.strip_prefix('=')?;

    let (first, second) = match value.split_once('-') {
        Some((first, second)) => (first, Some(second)),
        None => (value, None),
    };

    let first: u16 = first.trim().parse().ok()?;

    let second = match second {
        Some(second) => second.trim().parse().ok()?,
        None => first.checked_add(1)?,
    };

    Some((first, second))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tcp_interleaved_channels_are_read_from_the_header() {
        assert_eq!(
            Transport::negotiate("RTP/AVP/TCP;unicast;interleaved=0-1"),
            Some(Transport::Interleaved {
                rtp_channel: 0,
                rtcp_channel: 1
            })
        );
    }

    #[test]
    fn non_default_interleaved_channels_are_honoured() {
        // Un client qui ouvre plusieurs pistes sur une connexion décale les
        // canaux : les ignorer enverrait la vidéo sur le canal de l'audio.
        assert_eq!(
            Transport::negotiate("RTP/AVP/TCP;unicast;interleaved=4-5"),
            Some(Transport::Interleaved {
                rtp_channel: 4,
                rtcp_channel: 5
            })
        );
    }

    #[test]
    fn tcp_without_interleaved_falls_back_to_channels_zero_and_one() {
        assert_eq!(
            Transport::negotiate("RTP/AVP/TCP;unicast"),
            Some(Transport::Interleaved {
                rtp_channel: 0,
                rtcp_channel: 1
            })
        );
    }

    #[test]
    fn udp_client_ports_are_read_from_the_header() {
        assert_eq!(
            Transport::negotiate("RTP/AVP;unicast;client_port=5000-5001"),
            Some(Transport::Udp {
                rtp_port: 5000,
                rtcp_port: 5001
            })
        );
    }

    #[test]
    fn an_explicit_udp_profile_is_recognized_too() {
        assert_eq!(
            Transport::negotiate("RTP/AVP/UDP;unicast;client_port=6000-6001"),
            Some(Transport::Udp {
                rtp_port: 6000,
                rtcp_port: 6001
            })
        );
    }

    #[test]
    fn a_single_client_port_implies_the_next_one_for_rtcp() {
        assert_eq!(
            Transport::negotiate("RTP/AVP;unicast;client_port=5000"),
            Some(Transport::Udp {
                rtp_port: 5000,
                rtcp_port: 5001
            })
        );
    }

    #[test]
    fn udp_without_client_ports_is_refused() {
        // Il n'y aurait nulle part où envoyer le flux : mieux vaut refuser
        // que de diffuser dans le vide.
        assert_eq!(Transport::negotiate("RTP/AVP;unicast"), None);
    }

    #[test]
    fn multicast_is_refused() {
        assert_eq!(
            Transport::negotiate("RTP/AVP;multicast;port=5000-5001"),
            None
        );
    }

    #[test]
    fn encrypted_and_feedback_profiles_are_refused() {
        assert_eq!(
            Transport::negotiate("RTP/SAVP;unicast;client_port=5000-5001"),
            None
        );
    }

    #[test]
    fn the_first_servable_proposal_wins_over_the_clients_later_choices() {
        // VLC propose souvent UDP puis TCP : on respecte son ordre de
        // préférence.
        assert_eq!(
            Transport::negotiate(
                "RTP/AVP/UDP;unicast;client_port=5000-5001,RTP/AVP/TCP;unicast;interleaved=0-1"
            ),
            Some(Transport::Udp {
                rtp_port: 5000,
                rtcp_port: 5001
            })
        );
    }

    #[test]
    fn an_unservable_first_proposal_falls_through_to_the_next() {
        // Multicast d'abord, puis TCP : on doit retenir le TCP et non
        // refuser tout l'en-tête.
        assert_eq!(
            Transport::negotiate("RTP/AVP;multicast,RTP/AVP/TCP;unicast;interleaved=2-3"),
            Some(Transport::Interleaved {
                rtp_channel: 2,
                rtcp_channel: 3
            })
        );
    }

    #[test]
    fn an_unparseable_header_yields_no_transport() {
        assert_eq!(Transport::negotiate("n'importe quoi"), None);
        assert_eq!(Transport::negotiate(""), None);
    }

    #[test]
    fn the_interleaved_response_echoes_the_channels_and_the_ssrc() {
        let transport = Transport::Interleaved {
            rtp_channel: 0,
            rtcp_channel: 1,
        };

        assert_eq!(
            transport.response_header(0xDEAD_BEEF, None),
            "RTP/AVP/TCP;unicast;interleaved=0-1;ssrc=DEADBEEF"
        );
    }

    #[test]
    fn the_udp_response_announces_the_server_ports() {
        // Le client y adresse ses rapports RTCP : sans eux, il ne sait pas
        // d'où vient le flux.
        let transport = Transport::Udp {
            rtp_port: 5000,
            rtcp_port: 5001,
        };

        assert_eq!(
            transport.response_header(0x0000_0001, Some((41000, 41001))),
            "RTP/AVP;unicast;client_port=5000-5001;server_port=41000-41001;ssrc=00000001"
        );
    }

    #[test]
    fn a_port_pair_at_the_top_of_the_range_does_not_overflow() {
        // `client_port=65535` ne peut pas impliquer 65536 pour RTCP.
        assert_eq!(
            Transport::negotiate("RTP/AVP;unicast;client_port=65535"),
            None
        );
    }

    #[test]
    fn port_zero_is_refused() {
        assert_eq!(
            Transport::negotiate("RTP/AVP;unicast;client_port=0-1"),
            None
        );
    }
}
