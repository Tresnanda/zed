impl MetalRenderer {
    // Preserve ordering and original GPU slots. Skip only bounded primitive
    // ranges that cannot touch the current scissor; transformed sprites,
    // shadows and uncommon primitive kinds remain conservative.
    fn paint_region_batches<'a>(scene:&'a Scene,batch:PrimitiveBatch,rect:Option<metal::MTLScissorRect>) -> impl Iterator<Item=PrimitiveBatch>+'a {
        static CULL:std::sync::OnceLock<bool>=std::sync::OnceLock::new();
        let rect=if *CULL.get_or_init(||std::env::var("KYBERN_REGION_CULL").as_deref()!=Ok("0")){rect}else{None};
        let mut pending=Some(batch);
        std::iter::from_fn(move||{
            let batch=pending.take()?;
            let Some(rect)=rect else{return Some(batch);};
            let touches=|b:&Bounds<ScaledPixels>|{
                b.origin.x.0+b.size.width.0+2.>rect.x as f32 && b.origin.y.0+b.size.height.0+2.>rect.y as f32
                    && b.origin.x.0<(rect.x+rect.width) as f32+2. && b.origin.y.0<(rect.y+rect.height) as f32+2.
            };
            match batch {
                PrimitiveBatch::Quads(range)=>Self::paint_region_run(range,|i|touches(&scene.quads[i].bounds)).map(|(run,tail)|{pending=Some(PrimitiveBatch::Quads(tail));PrimitiveBatch::Quads(run)}),
                PrimitiveBatch::MonochromeSprites{texture_id,range}=>Self::paint_region_run(range,|i|scene.monochrome_sprites[i].transformation!=gpui::TransformationMatrix::unit()||touches(&scene.monochrome_sprites[i].bounds)).map(|(run,tail)|{pending=Some(PrimitiveBatch::MonochromeSprites{texture_id,range:tail});PrimitiveBatch::MonochromeSprites{texture_id,range:run}}),
                PrimitiveBatch::PolychromeSprites{texture_id,range}=>Self::paint_region_run(range,|i|touches(&scene.polychrome_sprites[i].bounds)).map(|(run,tail)|{pending=Some(PrimitiveBatch::PolychromeSprites{texture_id,range:tail});PrimitiveBatch::PolychromeSprites{texture_id,range:run}}),
                other=>Some(other),
            }
        })
    }
    fn paint_region_run(range:Range<usize>,touches:impl Fn(usize)->bool) -> Option<(Range<usize>,Range<usize>)> {
        let start=(range.start..range.end).find(|&index|touches(index))?;
        let end=(start+1..range.end).find(|&index|!touches(index)).unwrap_or(range.end);
        Some((start..end,end..range.end))
    }
}
