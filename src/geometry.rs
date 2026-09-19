//! Primitives géométriques partagées entre le tracking de personnes
//! ([`crate::capture`]) et la détection de visages ([`crate::vision`]), pour
//! éviter de dupliquer le calcul d'intersection sur union (IoU) dans chaque
//! module.

/// Calcule l'intersection sur union (IoU) entre deux rectangles donnés sous
/// la forme `(x, y, largeur, hauteur)`. Retourne 0.0 si les rectangles ne se
/// chevauchent pas (ou si l'un des deux a une aire nulle).
pub fn iou(a: (f32, f32, f32, f32), b: (f32, f32, f32, f32)) -> f32 {
    let (ax, ay, aw, ah) = a;
    let (bx, by, bw, bh) = b;

    let ax2 = ax + aw;
    let ay2 = ay + ah;

    let bx2 = bx + bw;
    let by2 = by + bh;

    let x1 = ax.max(bx);
    let y1 = ay.max(by);

    let x2 = ax2.min(bx2);
    let y2 = ay2.min(by2);

    if x2 <= x1 || y2 <= y1 {
        return 0.0;
    }

    let intersection = (x2 - x1) * (y2 - y1);

    let area_a = aw * ah;
    let area_b = bw * bh;

    let union = area_a + area_b - intersection;

    if union <= 0.0 {
        0.0
    } else {
        intersection / union
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_rects_have_iou_of_one() {
        let rect = (10.0, 10.0, 50.0, 30.0);
        assert_eq!(iou(rect, rect), 1.0);
    }

    #[test]
    fn disjoint_rects_have_iou_of_zero() {
        let a = (0.0, 0.0, 10.0, 10.0);
        let b = (100.0, 100.0, 10.0, 10.0);
        assert_eq!(iou(a, b), 0.0);
    }

    #[test]
    fn touching_edges_have_iou_of_zero() {
        // Deux rectangles qui se touchent exactement sur un bord (aucune
        // surface commune) : x2 == x1, donc rejeté par la condition
        // `x2 <= x1`.
        let a = (0.0, 0.0, 10.0, 10.0);
        let b = (10.0, 0.0, 10.0, 10.0);
        assert_eq!(iou(a, b), 0.0);
    }

    #[test]
    fn partial_overlap_matches_expected_ratio() {
        // a = [0,10]x[0,10] (aire 100), b = [5,15]x[0,10] (aire 100)
        // intersection = [5,10]x[0,10] = aire 50
        // union = 100 + 100 - 50 = 150 -> iou = 50/150 = 1/3
        let a = (0.0, 0.0, 10.0, 10.0);
        let b = (5.0, 0.0, 10.0, 10.0);
        assert!((iou(a, b) - (1.0 / 3.0)).abs() < 1e-6);
    }

    #[test]
    fn zero_area_rect_has_iou_of_zero() {
        let a = (0.0, 0.0, 0.0, 0.0);
        let b = (0.0, 0.0, 10.0, 10.0);
        assert_eq!(iou(a, b), 0.0);
    }

    #[test]
    fn one_rect_fully_inside_another() {
        // b est entièrement contenu dans a : intersection = aire de b.
        let a = (0.0, 0.0, 100.0, 100.0);
        let b = (10.0, 10.0, 10.0, 10.0);
        // union = 10000 + 100 - 100 = 10000 -> iou = 100/10000 = 0.01
        assert!((iou(a, b) - 0.01).abs() < 1e-6);
    }

    #[test]
    fn iou_is_symmetric() {
        let a = (0.0, 0.0, 20.0, 20.0);
        let b = (10.0, 10.0, 20.0, 20.0);
        assert_eq!(iou(a, b), iou(b, a));
    }
}
