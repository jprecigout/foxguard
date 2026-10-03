//! Écriture des boîtes ISO-BMFF, la brique de base du format MP4
//! (ISO/IEC 14496-12).
//!
//! Un fichier MP4 n'est qu'un arbre de **boîtes**. Chacune commence par sa
//! taille totale sur 32 bits, puis un type sur quatre caractères, puis son
//! contenu — qui est souvent lui-même une suite de boîtes.
//!
//! ```text
//! [u32 taille][4 car. type][contenu...]
//! ```
//!
//! La taille INCLUT l'en-tête de huit octets, et n'est connue qu'une fois le
//! contenu écrit. C'est tout le problème que résout [`Mp4Box`] : on écrit
//! d'abord, on renseigne la taille ensuite, en revenant sur ses pas dans le
//! tampon. Calculer les tailles à l'avance obligerait à décrire deux fois
//! chaque boîte — une fois pour sa taille, une fois pour son contenu — et la
//! moindre divergence entre les deux produirait un fichier que personne ne
//! sait lire, sans rien pour le signaler.

/// Une boîte en cours d'écriture dans un tampon.
///
/// À refermer par [`Mp4Box::end`], qui renseigne rétroactivement la taille.
#[must_use = "une boîte non refermée laisse une taille nulle dans le fichier"]
pub(super) struct Mp4Box {
    /// Position de l'en-tête de taille dans le tampon.
    start: usize,
}

impl Mp4Box {
    /// Ouvre une boîte de type `kind` (quatre caractères ASCII).
    pub(super) fn start(out: &mut Vec<u8>, kind: &[u8; 4]) -> Self {
        let start = out.len();

        // Taille provisoire : renseignée par `end`, une fois le contenu écrit.
        out.extend_from_slice(&[0, 0, 0, 0]);
        out.extend_from_slice(kind);

        Self { start }
    }

    /// Ouvre une « full box » : une boîte ordinaire suivie d'un octet de
    /// version et de trois octets de drapeaux.
    ///
    /// La plupart des boîtes de métadonnées en sont (`mvhd`, `tkhd`,
    /// `trun`, ...). Les oublier décale tout le contenu de quatre octets.
    pub(super) fn start_full(out: &mut Vec<u8>, kind: &[u8; 4], version: u8, flags: u32) -> Self {
        let boxed = Self::start(out, kind);

        out.push(version);
        // Les drapeaux tiennent sur 24 bits : on écarte l'octet de poids fort.
        out.extend_from_slice(&flags.to_be_bytes()[1..]);

        boxed
    }

    /// Referme la boîte en inscrivant sa taille réelle.
    ///
    /// Prend une TRANCHE et non le tampon : refermer une boîte ne fait que
    /// réécrire quatre octets déjà présents, jamais en ajouter.
    pub(super) fn end(self, out: &mut [u8]) {
        let size = (out.len() - self.start) as u32;
        out[self.start..self.start + 4].copy_from_slice(&size.to_be_bytes());
    }

    /// Position de cette boîte dans le tampon.
    ///
    /// Sert au seul endroit où une boîte doit connaître sa propre adresse :
    /// le décalage vers les données que porte `trun` est compté depuis le
    /// début du `moof` qui la contient (voir `super::writer`).
    pub(super) fn offset(&self) -> usize {
        self.start
    }
}

/// Écrit un entier 16 bits en ordre réseau.
pub(super) fn u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_be_bytes());
}

/// Écrit un entier 32 bits en ordre réseau.
pub(super) fn u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_be_bytes());
}

/// Écrit un entier 64 bits en ordre réseau.
pub(super) fn u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_be_bytes());
}

/// Écrit `count` octets nuls.
///
/// Les en-têtes MP4 comportent beaucoup de champs réservés, qui doivent être
/// présents et nuls. Les compter à la main dans chaque boîte serait la
/// première source d'erreurs d'alignement.
pub(super) fn zeros(out: &mut Vec<u8>, count: usize) {
    out.resize(out.len() + count, 0);
}

/// Matrice de transformation d'affichage, en virgule fixe 16.16.
///
/// L'identité : aucune rotation ni mise à l'échelle. Les lecteurs l'exigent
/// dans `mvhd` et `tkhd` ; une matrice nulle donne, selon le lecteur, une
/// image invisible ou une erreur.
pub(super) fn identity_matrix(out: &mut Vec<u8>) {
    const UNITY: u32 = 0x0001_0000;

    for value in [UNITY, 0, 0, 0, UNITY, 0, 0, 0, 0x4000_0000] {
        u32(out, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Taille déclarée par l'en-tête d'une boîte.
    fn declared_size(bytes: &[u8]) -> u32 {
        u32::from_be_bytes(bytes[0..4].try_into().unwrap())
    }

    #[test]
    fn an_empty_box_declares_its_eight_byte_header() {
        let mut out = Vec::new();
        Mp4Box::start(&mut out, b"free").end(&mut out);

        assert_eq!(out.len(), 8);
        assert_eq!(declared_size(&out), 8);
        assert_eq!(&out[4..8], b"free");
    }

    #[test]
    fn the_declared_size_includes_the_content() {
        let mut out = Vec::new();
        let boxed = Mp4Box::start(&mut out, b"mdat");
        out.extend_from_slice(&[1, 2, 3, 4, 5]);
        boxed.end(&mut out);

        assert_eq!(declared_size(&out), 13);
        assert_eq!(out.len(), 13);
    }

    #[test]
    fn a_full_box_carries_its_version_and_flags() {
        let mut out = Vec::new();
        // 0x020000 : « les décalages sont comptés depuis le moof », le
        // drapeau de `tfhd` dont dépend tout le placement des données.
        Mp4Box::start_full(&mut out, b"tfhd", 0, 0x02_0000).end(&mut out);

        assert_eq!(
            out.len(),
            12,
            "en-tête de 8 octets + version + 3 de drapeaux"
        );
        assert_eq!(out[8], 0, "version");
        assert_eq!(&out[9..12], &[0x02, 0x00, 0x00], "drapeaux sur 24 bits");
    }

    #[test]
    fn the_flags_of_a_full_box_use_all_twenty_four_bits() {
        // `trun` en utilise quatre à la fois (décalage, durée, taille,
        // propriétés) : un masque trop étroit en perdrait.
        let mut out = Vec::new();
        Mp4Box::start_full(&mut out, b"trun", 1, 0x0F_0701).end(&mut out);

        assert_eq!(&out[9..12], &[0x0F, 0x07, 0x01]);
        assert_eq!(out[8], 1, "version");
    }

    #[test]
    fn nested_boxes_each_declare_their_own_size() {
        // C'est l'imbrication qui fait tout le format : une erreur de taille
        // sur la boîte extérieure rend tout ce qui suit illisible.
        let mut out = Vec::new();

        let outer = Mp4Box::start(&mut out, b"moof");
        let inner = Mp4Box::start(&mut out, b"mfhd");
        u32(&mut out, 42);
        inner.end(&mut out);
        outer.end(&mut out);

        assert_eq!(declared_size(&out), 20, "moof : 8 + mfhd (12)");
        assert_eq!(declared_size(&out[8..]), 12, "mfhd : 8 + 4");
        assert_eq!(&out[12..16], b"mfhd");
    }

    #[test]
    fn a_box_knows_where_it_starts() {
        // Le décalage du `trun` vers les données est compté depuis le début
        // du `moof` : cette position est la seule façon de le calculer.
        let mut out = vec![0xAA; 7];
        let boxed = Mp4Box::start(&mut out, b"moof");

        assert_eq!(boxed.offset(), 7);
        boxed.end(&mut out);
    }

    #[test]
    fn integers_are_written_in_network_order() {
        let mut out = Vec::new();
        u16(&mut out, 0x0102);
        u32(&mut out, 0x0304_0506);
        u64(&mut out, 0x0708_090A_0B0C_0D0E);

        assert_eq!(
            out,
            vec![
                0x01, 0x02, //
                0x03, 0x04, 0x05, 0x06, //
                0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E,
            ]
        );
    }

    #[test]
    fn zeros_appends_exactly_what_is_asked() {
        let mut out = vec![0xFF];
        zeros(&mut out, 3);

        assert_eq!(out, vec![0xFF, 0, 0, 0]);
    }

    #[test]
    fn the_identity_matrix_has_the_shape_players_expect() {
        let mut out = Vec::new();
        identity_matrix(&mut out);

        assert_eq!(out.len(), 36, "neuf entiers de 32 bits");
        assert_eq!(&out[0..4], &0x0001_0000u32.to_be_bytes(), "a = 1.0");
        assert_eq!(&out[16..20], &0x0001_0000u32.to_be_bytes(), "d = 1.0");
        assert_eq!(&out[32..36], &0x4000_0000u32.to_be_bytes(), "w = 1.0");
    }
}
