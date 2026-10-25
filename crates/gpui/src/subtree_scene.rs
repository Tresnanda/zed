// Included in gpui::scene. Primitive payloads keep their atlas tiles, shadows,
// opacity and shape. Only spatial fields and the retained clip chain change.
impl Primitive {
    pub(crate) fn translate_subtree(&mut self, delta: Point<ScaledPixels>, mask: ContentMask<ScaledPixels>) {
        macro_rules! move_box {
            ($p:expr) => {{
                $p.bounds.origin += delta;
                $p.content_mask = mask;
            }};
        }
        match self {
            Self::Backdrop(p) => move_box!(p),
            Self::Shadow(p) => {
                move_box!(p);
                p.element_bounds.origin += delta;
            }
            Self::Quad(p) => move_box!(p),
            Self::Underline(p) => move_box!(p),
            Self::MonochromeSprite(p) => {
                move_box!(p);
                move_sprite_transform(&mut p.transformation, delta);
            }
            Self::SubpixelSprite(p) => {
                move_box!(p);
                move_sprite_transform(&mut p.transformation, delta);
            }
            Self::PolychromeSprite(p) => move_box!(p),
            Self::Surface(p) => move_box!(p),
            Self::Path(p) => {
                move_box!(p);
                for vertex in &mut p.vertices {
                    vertex.xy_position += delta;
                    vertex.content_mask = mask;
                }
            }
        }
    }
}
fn move_sprite_transform(matrix: &mut TransformationMatrix, delta: Point<ScaledPixels>) {
    let x = delta.x.0;
    let y = delta.y.0;
    matrix.translation[0] += x - matrix.rotation_scale[0][0] * x - matrix.rotation_scale[0][1] * y;
    matrix.translation[1] += y - matrix.rotation_scale[1][0] * x - matrix.rotation_scale[1][1] * y;
}
