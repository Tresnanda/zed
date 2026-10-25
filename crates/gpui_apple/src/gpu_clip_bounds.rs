// Conservative device-pixel containment. Leave a full pixel around every
// analytic edge, so CPU rounding cannot change fractional shader coverage.
fn clip_contains_rect(bounds: [f32; 4], clip: [f32; 4], radii: [f32; 4]) -> bool {
    if bounds.iter().chain(clip.iter()).chain(radii.iter()).any(|v| !v.is_finite()) { return false; }
    let [x,y,w,h] = bounds;
    let [left,top,width,height] = clip;
    let right = x+w;
    let bottom = y+h;
    if w <= 0. || h <= 0. || x < left+1. || y < top+1. || right > left+width-1. || bottom > top+height-1. { return false; }
    let radius = radii.into_iter().fold(0f32, f32::max)+1.;
    (x >= left+radius && right <= left+width-radius) ||
        (y >= top+radius && bottom <= top+height-radius)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_fully_covered_interiors() {
        let c=[10.,20.,100.,80.]; let r=[12.;4];
        assert!(clip_contains_rect([25.,22.,50.,4.],c,r));
        assert!(clip_contains_rect([12.,35.,4.,30.],c,r));
        assert!(!clip_contains_rect([11.,21.,8.,8.],c,r));
        assert!(!clip_contains_rect([10.,40.,10.,10.],c,r));
        assert!(!clip_contains_rect([25.,21.,90.,10.],c,r));
        assert!(!clip_contains_rect([f32::NAN,30.,1.,1.],c,r));
        assert!(!clip_contains_rect([25.,30.,0.,1.],c,r));
        assert!(!clip_contains_rect([14.,22.,4.,4.],c,[2.,2.,20.,2.]));
    }
}
