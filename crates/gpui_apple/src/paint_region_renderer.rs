// A retained color target is used only while a native passive-paint loop is
// visible. Ordinary scenes keep the original direct-to-drawable path.
struct PaintRegionResources {
    target: Option<metal::Texture>,
    viewport: (u64,u64),
    atlas: Option<PaintRegionAtlas>,
    generation: u64,
    clear: metal::RenderPipelineState,
    copy: metal::RenderPipelineState,
    holes: metal::RenderPipelineState,
    layers: Vec<PaintRegionLayer>,
    regions: Vec<metal::MTLScissorRect>,
}
impl MetalRenderer {
    /// Parked windows retain no passive-animation color target. The next draw
    /// reconstructs it from the complete scene before applying any damage.
    #[track_caller]
    pub fn release_passive_region_target(&mut self) {
        if self.paint_region_resources.is_some() && std::env::var_os("KYBERN_DEBUG_REGION").is_some() { eprintln!("[paint-region] release {}",std::panic::Location::caller()); }
        let had_layers=self.has_passive_region_layers();
        self.paint_region_scissor = None;
        self.paint_region_resources = None;
        if had_layers {if let Some(layer)=self.layer.as_ref(){let mode:bool=unsafe{msg_send![&**layer,presentsWithTransaction]};if mode!=self.presents_with_transaction{layer.set_presents_with_transaction(self.presents_with_transaction);}}}
    }
    fn passive_region_enabled() -> bool {
        static ENABLED:std::sync::OnceLock<bool>=std::sync::OnceLock::new();
        *ENABLED.get_or_init(||std::env::var("KYBERN_REGION_PAINT").as_deref()==Ok("1"))
    }
    fn has_passive_region_layers(&self)->bool {
        self.paint_region_resources.as_ref().is_some_and(|r|!r.layers.is_empty()||r.atlas.as_ref().is_some_and(|a|!a.layers.is_empty()))
    }
    fn paint_region_target(&mut self,scene:&Scene,viewport:Size<DevicePixels>)->Result<Option<metal::Texture>> {
        if Self::paint_atlas_enabled()&&self.is_unified_memory&&self.is_apple_gpu {self.paint_region_scissor=None;return Ok(None);}
        if !self.prepare_passive_region(scene,viewport,true)? {return Ok(None);}
        Ok(self.paint_region_resources.as_ref().and_then(|r|r.target.clone()))
    }
    fn prepare_passive_region(&mut self, scene: &Scene, viewport: Size<DevicePixels>, target_required:bool) -> Result<bool> {
        let enabled=Self::passive_region_enabled();
        // Paths use a second full-size render pass. Filters sample surrounding
        // pixels. Keep those scenes on their established full-render path.
        if !enabled || !scene.has_native_paint_animation() || !scene.paths.is_empty() || !scene.backdrops.is_empty() || !scene.surfaces.is_empty() {
            self.release_passive_region_target();
            return Ok(false);
        }
        let width = i32::from(viewport.width).max(1) as u64;
        let height = i32::from(viewport.height).max(1) as u64;
        if self.paint_region_resources.as_ref().is_none_or(|r| r.viewport!=(width,height) || (target_required&&r.target.is_none())) {
            if std::env::var_os("KYBERN_DEBUG_REGION").is_some() { eprintln!("[paint-region] prepare {width}x{height} target={target_required}"); }
            let library = self.device.new_library_with_source(include_str!("paint_region.metal"), &metal::CompileOptions::new()).map_err(anyhow::Error::msg)?;
            let vertex = library.get_function("region_vertex",None).map_err(anyhow::Error::msg)?;
            let pipeline = |name: &str| -> Result<metal::RenderPipelineState> {
                let fragment = library.get_function(name,None).map_err(anyhow::Error::msg)?;
                let desc = metal::RenderPipelineDescriptor::new();
                desc.set_vertex_function(Some(&vertex)); desc.set_fragment_function(Some(&fragment));
                let attachment = desc.color_attachments().object_at(0).unwrap();
                attachment.set_pixel_format(MTLPixelFormat::BGRA8Unorm);
                attachment.set_blending_enabled(false);
                self.device.new_render_pipeline_state(&desc).map_err(anyhow::Error::msg)
            };
            let desc = metal::TextureDescriptor::new();
            desc.set_width(width); desc.set_height(height); desc.set_pixel_format(MTLPixelFormat::BGRA8Unorm);
            desc.set_usage(metal::MTLTextureUsage::RenderTarget | metal::MTLTextureUsage::ShaderRead);
            desc.set_storage_mode(metal::MTLStorageMode::Private);
            self.paint_region_resources = Some(PaintRegionResources { target: target_required.then(||self.device.new_texture(&desc)), viewport:(width,height), atlas:None, generation: 0, clear: pipeline("region_clear")?, copy: pipeline("region_copy")?, holes: pipeline("region_copy_holes")?, layers: Vec::new(), regions: Vec::new() });
        }
        let resources = self.paint_region_resources.as_mut().unwrap();
        self.paint_region_scissor = if resources.generation == scene.paint_generation {
            scene.passive_paint_bounds.map(|bounds| {
                let left = bounds.origin.x.0.floor().max(0.0).min(width as f32) as u64;
                let top = bounds.origin.y.0.floor().max(0.0).min(height as f32) as u64;
                let right = (bounds.origin.x.0+bounds.size.width.0).ceil().max(left as f32).min(width as f32) as u64;
                let bottom = (bounds.origin.y.0+bounds.size.height.0).ceil().max(top as f32).min(height as f32) as u64;
                metal::MTLScissorRect { x:left, y:top, width:right-left, height:bottom-top }
            }).filter(|r| r.width>0 && r.height>0)
        } else { None };
        static REPORTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if self.paint_region_scissor.is_some() && std::env::var_os("KYBERN_DEBUG_REGION").is_some() && !REPORTED.swap(true,std::sync::atomic::Ordering::Relaxed) {
            let r=self.paint_region_scissor.unwrap();eprintln!("[paint-region] scissor {},{} {}x{}",r.x,r.y,r.width,r.height);
        }
        Ok(true)
    }

    fn paint_region_rectangles(&self, scene: &Scene, viewport: Size<DevicePixels>) -> Vec<Option<metal::MTLScissorRect>> {
        if self.paint_region_origin.is_some(){return if self.paint_region_crops.is_empty(){vec![self.paint_region_scissor]}else{self.paint_region_crops.iter().copied().map(Some).collect()};}
        let Some(union) = self.paint_region_scissor else { return vec![None]; };
        if std::env::var("KYBERN_REGION_RECTS").as_deref() == Ok("0") { return vec![Some(union)]; }
        let width=i32::from(viewport.width).max(1) as u64;
        let height=i32::from(viewport.height).max(1) as u64;
        let mut rectangles:Vec<metal::MTLScissorRect>=Vec::new();
        for bounds in scene.passive_paint_regions.values() {
            let x=bounds.origin.x.0.floor().max(0.).min(width as f32) as u64;
            let y=bounds.origin.y.0.floor().max(0.).min(height as f32) as u64;
            let right=(bounds.origin.x.0+bounds.size.width.0).ceil().max(x as f32).min(width as f32) as u64;
            let bottom=(bounds.origin.y.0+bounds.size.height.0).ceil().max(y as f32).min(height as f32) as u64;
            if right==x || bottom==y {continue;}
            let mut rect=metal::MTLScissorRect{x,y,width:right-x,height:bottom-y};
            // Coalesce overlaps (including matrix dots sharing GPU tiles).
            // Remove/restart makes the result disjoint even for bridge regions.
            let mut index=0;
            while index<rectangles.len() {
                let old=rectangles[index];
                if rect.x<=old.x+old.width+8 && old.x<=rect.x+rect.width+8 && rect.y<=old.y+old.height+8 && old.y<=rect.y+rect.height+8 {
                    let right=(rect.x+rect.width).max(old.x+old.width);
                    let bottom=(rect.y+rect.height).max(old.y+old.height);
                    rect.x=rect.x.min(old.x);rect.y=rect.y.min(old.y);
                    rect.width=right-rect.x;rect.height=bottom-rect.y;
                    rectangles.swap_remove(index);index=0;
                } else {index+=1;}
            }
            rectangles.push(rect);
        }
        if rectangles.len()>16 { return vec![Some(union)]; }
        if rectangles.is_empty() {vec![Some(union)]} else {rectangles.into_iter().map(Some).collect()}
    }

    fn trace_paint_region(&self, command: &metal::CommandBufferRef, scene: &Scene, viewport: Size<DevicePixels>, retained: bool, stage: &'static str) {
        static TRACE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if !*TRACE.get_or_init(|| std::env::var("KYBERN_GPU_TRACE").as_deref() == Ok("1")) { return; }
        let regions=self.paint_region_rectangles(scene,viewport);
        let region_count=regions.len();
        let pixels:u64=regions.iter().map(|r|r.map_or(i32::from(viewport.width).max(0) as u64*i32::from(viewport.height).max(0) as u64,|r|r.width*r.height)).sum();
        let generation=scene.paint_generation;
        let rect=self.paint_region_scissor.map(|r|(r.x,r.y,r.width,r.height));
        let width=i32::from(viewport.width); let height=i32::from(viewport.height);
        let paths=scene.paths.len(); let backdrops=scene.backdrops.len();
        let quads=scene.quads.len(); let glyphs=scene.monochrome_sprites.len();
        let block=ConcreteBlock::new(move |buffer: &metal::CommandBufferRef| {
            let start:f64=unsafe {msg_send![buffer, GPUStartTime]};
            let end:f64=unsafe {msg_send![buffer, GPUEndTime]};
            let message=format!("[paint-gpu] stage={} ms={:.6} generation={} retained={} regions={} pixels={} rect={:?} viewport={}x{} paths={} backdrops={} quads={} glyphs={}\n", stage,(end-start)*1000.,generation,retained,region_count,pixels,rect,width,height,paths,backdrops,quads,glyphs);
            let _=std::io::Write::write_all(&mut std::io::stderr().lock(),message.as_bytes());
        }).copy();
        command.add_completed_handler(&block);
    }

    fn clear_paint_region(&self, encoder: &metal::RenderCommandEncoderRef, alpha: f32) {
        let Some(mut scissor)=self.paint_region_scissor else{return;};
        let Some(resources)=self.paint_region_resources.as_ref() else{return;};
        let alpha=if let Some((x,y))=self.paint_region_origin {
            if !self.opaque{return;}
            scissor.x=(scissor.x as i64-x).max(0) as u64;scissor.y=(scissor.y as i64-y).max(0) as u64;
            1.
        }else{alpha};
        encoder.set_scissor_rect(scissor);
        encoder.set_render_pipeline_state(&resources.clear);
        encoder.set_fragment_bytes(0,mem::size_of_val(&alpha) as u64,&alpha as *const f32 as *const _);
        encoder.draw_primitives(metal::MTLPrimitiveType::Triangle,0,3);
    }
    fn clear_base_holes(&self,encoder:&metal::RenderCommandEncoderRef){
        let Some(resources)=self.paint_region_resources.as_ref() else{return;};
        let alpha=0f32;
        encoder.set_render_pipeline_state(&resources.clear);
        encoder.set_fragment_bytes(0,mem::size_of_val(&alpha) as u64,&alpha as *const f32 as *const _);
        for rect in &self.paint_region_base_holes {encoder.set_scissor_rect(*rect);encoder.draw_primitives(metal::MTLPrimitiveType::Triangle,0,3);}
    }

    fn copy_paint_region(&self, command: &metal::CommandBufferRef, target: &metal::TextureRef, destination: &metal::TextureRef, viewport: Size<DevicePixels>) {
        let resources = self.paint_region_resources.as_ref().unwrap();
        // The fullscreen triangle overwrites every drawable pixel with blending
        // disabled. Loading its previous contents costs a full-target read and
        // makes no contribution to the result (drawable contents are undefined).
        let pass = metal::RenderPassDescriptor::new();
        let color = pass.color_attachments().object_at(0).unwrap();
        color.set_texture(Some(destination));
        static LOAD: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        color.set_load_action(if *LOAD.get_or_init(|| std::env::var("KYBERN_REGION_COPY_LOAD").as_deref() == Ok("1")) {metal::MTLLoadAction::Load} else {metal::MTLLoadAction::DontCare});
        color.set_store_action(metal::MTLStoreAction::Store);
        let encoder = command.new_render_command_encoder(pass);
        encoder.set_viewport(metal::MTLViewport {originX:0.,originY:0.,width:i32::from(viewport.width) as f64,height:i32::from(viewport.height) as f64,znear:0.,zfar:1.});
        encoder.set_render_pipeline_state(&resources.copy);
        encoder.set_fragment_texture(0,Some(target));
        let origin=[0u32;2];
        encoder.set_fragment_bytes(1,mem::size_of_val(&origin) as u64,origin.as_ptr() as *const _);
        encoder.draw_primitives(metal::MTLPrimitiveType::Triangle,0,3);
        encoder.end_encoding();
    }
}
