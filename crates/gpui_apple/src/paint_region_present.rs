// A passive loop changes small, fixed portions of an otherwise settled
// scene. Keep the base drawable displayed and present only those portions in
// child Metal layers. Geometry, filters and ordinary scene changes still render
// the complete base layer. This experimental path is explicitly opt-in.
struct PaintRegionLayer {
    layer: metal::MetalLayer,
    rect: metal::MTLScissorRect,
}
impl Drop for PaintRegionLayer {
    fn drop(&mut self) {
        unsafe { let _: () = msg_send![&*self.layer, removeFromSuperlayer]; }
    }
}
impl MetalRenderer {
    fn render_cropped_regions(&mut self,scene:&Scene,viewport:Size<DevicePixels>,drawables:&[(metal::MetalDrawable,metal::MTLScissorRect)],regions:&[metal::MTLScissorRect],existing:Option<&metal::CommandBufferRef>) -> Result<metal::CommandBuffer> {
        let mut writer=InstanceBufferWriter::new(&self.device,&self.instance_buffer_pool,self.is_unified_memory);
        let bindings=write_instances(scene,&mut writer)?;
        let command=existing.map(|command|command.to_owned()).unwrap_or_else(||self.command_queue.new_command_buffer().to_owned());
        let damage=self.paint_region_scissor;
        self.paint_region_command=Some(command.clone());
        self.paint_region_crops=regions.to_vec();
        let result=(||->Result<()>{
            for (drawable,rect) in drawables {
                self.paint_region_origin=Some((rect.x as i64,rect.y as i64));
                self.paint_region_scissor=Some(*rect);
                self.draw_primitives_to_texture(scene,&bindings,&mut writer,drawable.texture(),viewport)?;
            }
            Ok(())
        })();
        self.paint_region_origin=None;
        self.paint_region_scissor=damage;
        self.paint_region_command=None;
        self.paint_region_crops.clear();
        let pool=self.instance_buffer_pool.clone();
        let buffer=Cell::new(Some(writer.finish()));
        let block=ConcreteBlock::new(move |_|{if let Some(buffer)=buffer.take(){pool.lock().release(buffer);}}).copy();
        command.add_completed_handler(&block);
        if let Err(error)=result {command.commit();return Err(error);}
        Ok(command)
    }

    fn clear_passive_region_layers(&mut self) {
        if let Some(resources)=self.paint_region_resources.as_mut() {
            if !resources.layers.is_empty() || resources.atlas.is_some() {
                unsafe { let _: () = msg_send![objc::class!(CATransaction), begin];
                    let _: () = msg_send![objc::class!(CATransaction), setDisableActions: YES]; }
                resources.layers.clear();
                resources.atlas=None;
                unsafe { let _: () = msg_send![objc::class!(CATransaction), commit]; }
            }
        }
    }

    fn present_passive_regions(&mut self, scene: &Scene, viewport: Size<DevicePixels>) -> bool {
        static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if !*ENABLED.get_or_init(||std::env::var("KYBERN_REGION_LAYERS").as_deref()==Ok("1")) { return false; }
        if Self::paint_atlas_enabled() {return self.present_packed_regions(scene,viewport);}
        let target=match self.paint_region_target(scene,viewport) {
            Ok(Some(target))=>target,
            _=>return false,
        };
        let reconstruct=self.paint_region_scissor.is_none();
        let damage=self.paint_region_scissor;
        if reconstruct {
            let Some(bounds)=scene.passive_paint_bounds else {return false;};
            let width=i32::from(viewport.width).max(1) as u64;
            let height=i32::from(viewport.height).max(1) as u64;
            let x=bounds.origin.x.0.floor().max(0.).min(width as f32) as u64;
            let y=bounds.origin.y.0.floor().max(0.).min(height as f32) as u64;
            let right=(bounds.origin.x.0+bounds.size.width.0).ceil().max(x as f32).min(width as f32) as u64;
            let bottom=(bounds.origin.y.0+bounds.size.height.0).ceil().max(y as f32).min(height as f32) as u64;
            self.paint_region_scissor=Some(metal::MTLScissorRect{x,y,width:right-x,height:bottom-y});
        }
        let mut rectangles:Vec<_>=self.paint_region_rectangles(scene,viewport).into_iter().flatten().collect();
        self.paint_region_scissor=damage;
        rectangles.sort_by_key(|r|(r.y,r.x,r.width,r.height));
        if rectangles.is_empty(){return false;}
        static DIRECT:std::sync::OnceLock<bool>=std::sync::OnceLock::new();
        static SINGLE:std::sync::OnceLock<bool>=std::sync::OnceLock::new();
        let direct_enabled=*DIRECT.get_or_init(||std::env::var("KYBERN_REGION_DIRECT").as_deref()!=Ok("0"));
        let single=direct_enabled&&*SINGLE.get_or_init(||std::env::var("KYBERN_REGION_SINGLE_LAYER").as_deref()!=Ok("0"));
        let layer_rectangles=if single {
            let x=rectangles.iter().map(|r|r.x).min().unwrap();let y=rectangles.iter().map(|r|r.y).min().unwrap();
            let right=rectangles.iter().map(|r|r.x+r.width).max().unwrap();let bottom=rectangles.iter().map(|r|r.y+r.height).max().unwrap();
            vec![metal::MTLScissorRect{x,y,width:right-x,height:bottom-y}]
        } else {rectangles.clone()};
        let Some(parent)=self.layer.clone() else {return false;};
        let resources=self.paint_region_resources.as_mut().unwrap();
        let same=resources.layers.len()==layer_rectangles.len() && resources.layers.iter().zip(&layer_rectangles).all(|(a,b)|a.rect.x==b.x&&a.rect.y==b.y&&a.rect.width==b.width&&a.rect.height==b.height);
        let same_regions=resources.regions.len()==rectangles.len()&&resources.regions.iter().zip(&rectangles).all(|(a,b)|a.x==b.x&&a.y==b.y&&a.width==b.width&&a.height==b.height);
        if !same {
            unsafe { let _: () = msg_send![objc::class!(CATransaction), begin];
                let _: () = msg_send![objc::class!(CATransaction), setDisableActions: YES]; }
            resources.layers.clear();
            let bounds:cocoa::foundation::NSRect=unsafe {msg_send![&*parent, bounds]};
            if bounds.size.width<=0. || bounds.size.height<=0. {unsafe {let _: ()=msg_send![objc::class!(CATransaction),commit];}return false;}
            let scale_x=f64::from(i32::from(viewport.width))/bounds.size.width;
            let scale_y=f64::from(i32::from(viewport.height))/bounds.size.height;
            let flipped:bool=unsafe {msg_send![&*parent, isGeometryFlipped]};
            for rect in &layer_rectangles {
                let layer=metal::MetalLayer::new();
                layer.set_device(&self.device);
                layer.set_pixel_format(MTLPixelFormat::BGRA8Unorm);
                layer.set_opaque(false);
                layer.set_maximum_drawable_count(3);
                layer.set_presents_with_transaction(self.presents_with_transaction);
                let frame=cocoa::foundation::NSRect::new(
                    cocoa::foundation::NSPoint::new(bounds.origin.x+rect.x as f64/scale_x,
                        bounds.origin.y+if flipped {rect.y as f64/scale_y} else {bounds.size.height-(rect.y+rect.height) as f64/scale_y}),
                    NSSize::new(rect.width as f64/scale_x,rect.height as f64/scale_y));
                unsafe {
                    let _: ()=msg_send![&*layer,setFrame:frame];
                    let _: ()=msg_send![&*layer,setContentsScale:scale_x];
                    let _: ()=msg_send![&*layer,setDrawableSize:NSSize::new(rect.width as f64,rect.height as f64)];
                    let _: ()=msg_send![&*layer,setAllowsNextDrawableTimeout:YES];
                    let _: ()=msg_send![&*parent,addSublayer:&*layer];
                }
                resources.layers.push(PaintRegionLayer{layer,rect:*rect});
            }
            unsafe {let _: ()=msg_send![objc::class!(CATransaction),commit];}
            if std::env::var_os("KYBERN_DEBUG_REGION").is_some() {eprintln!("[paint-region] layers {}",layer_rectangles.len());}
        }
        // Punch transparent holes in the base only when it changes. Cropped
        // layers replace these pixels, preserving translucent AppKit materials.
        let base=if reconstruct||!same||!same_regions {
            let Some(drawable)=parent.next_drawable() else {return false;};
            Some(drawable.to_owned())
        } else {None};
        let mut drawables=Vec::new();
        for region in &resources.layers {
            let Some(drawable)=region.layer.next_drawable() else {return false;};
            drawables.push((drawable.to_owned(),region.rect));
        }
        let direct=direct_enabled&&!reconstruct;
        let rendered=if single {
            if reconstruct {self.render_frame(scene,&target,viewport).and_then(|command|self.render_cropped_regions(scene,viewport,&drawables,&rectangles,Some(&command)))}
            else {self.render_cropped_regions(scene,viewport,&drawables,&rectangles,None)}
        } else if direct {self.render_cropped_regions(scene,viewport,&drawables,&[],None)} else {self.render_frame(scene,&target,viewport)};
        let command=match rendered {
            Ok(command)=>command,
            Err(error)=>{log::error!("passive region layer: {error:#}");return false;},
        };
        let resources=self.paint_region_resources.as_mut().unwrap();
        if let Some(drawable)=base.as_ref() {
            let pass=metal::RenderPassDescriptor::new();
            let color=pass.color_attachments().object_at(0).unwrap();
            color.set_texture(Some(drawable.texture()));
            color.set_load_action(metal::MTLLoadAction::DontCare);
            color.set_store_action(metal::MTLStoreAction::Store);
            let encoder=command.new_render_command_encoder(pass);
            encoder.set_viewport(metal::MTLViewport{originX:0.,originY:0.,width:i32::from(viewport.width) as f64,height:i32::from(viewport.height) as f64,znear:0.,zfar:1.});
            encoder.set_render_pipeline_state(&resources.holes);
            encoder.set_fragment_texture(0,Some(&target));
            let mut holes=[[0u32;4];16];
            for (index,rect) in rectangles.iter().enumerate(){holes[index]=[rect.x as u32,rect.y as u32,(rect.x+rect.width) as u32,(rect.y+rect.height) as u32];}
            let count=rectangles.len() as u32;
            encoder.set_fragment_bytes(2,mem::size_of_val(&holes) as u64,holes.as_ptr() as *const _);
            encoder.set_fragment_bytes(3,mem::size_of_val(&count) as u64,&count as *const u32 as *const _);
            encoder.draw_primitives(metal::MTLPrimitiveType::Triangle,0,3);
            encoder.end_encoding();
        }
        if !direct&&!single {for (drawable,rect) in &drawables {
            let pass=metal::RenderPassDescriptor::new();
            let color=pass.color_attachments().object_at(0).unwrap();
            color.set_texture(Some(drawable.texture()));
            color.set_load_action(metal::MTLLoadAction::DontCare);
            color.set_store_action(metal::MTLStoreAction::Store);
            let encoder=command.new_render_command_encoder(pass);
            encoder.set_viewport(metal::MTLViewport{originX:0.,originY:0.,width:rect.width as f64,height:rect.height as f64,znear:0.,zfar:1.});
            encoder.set_render_pipeline_state(&resources.copy);
            encoder.set_fragment_texture(0,Some(&target));
            let origin=[rect.x as u32,rect.y as u32];
            encoder.set_fragment_bytes(1,mem::size_of_val(&origin) as u64,origin.as_ptr() as *const _);
            encoder.draw_primitives(metal::MTLPrimitiveType::Triangle,0,3);
            encoder.end_encoding();
        }
        }
        resources.regions=rectangles;
        resources.generation=scene.paint_generation;
        self.trace_paint_region(&command,scene,viewport,true,if direct {"crop"}else{"layers"});
        self.paint_region_scissor=None;
        if self.presents_with_transaction {
            command.commit();command.wait_until_scheduled();
            if let Some(drawable)=base.as_ref(){drawable.present();}
            for (drawable,_) in &drawables {drawable.present();}
        } else {
            if let Some(drawable)=base.as_ref(){command.present_drawable(drawable);}
            for (drawable,_) in &drawables {command.present_drawable(drawable);}
            command.commit();
        }
        true
    }
}
