// Pack disjoint damage into one small GPU target. Plain CA layers select their
// tiles from the same IOSurface, avoiding per-region drawable presentation and
// the large transparent union target. All mutations remain on GPUI's UI thread.
#[link(name="IOSurface",kind="framework")]
unsafe extern "C" {
    fn IOSurfaceCreate(properties:*const c_void)->*mut c_void;
    fn IOSurfaceIsInUse(surface:*mut c_void)->u8;
    static kIOSurfaceWidth:*const c_void;
    static kIOSurfaceHeight:*const c_void;
    static kIOSurfaceBytesPerElement:*const c_void;
    static kIOSurfacePixelFormat:*const c_void;
}
struct PaintAtlasSurface(*mut c_void);
impl Drop for PaintAtlasSurface {
    fn drop(&mut self){unsafe{core_foundation::base::CFRelease(self.0);}}
}
struct PaintSurfaceLayer {layer:*mut objc::runtime::Object,rect:metal::MTLScissorRect}
impl Drop for PaintSurfaceLayer {
    fn drop(&mut self){unsafe{let _:()=msg_send![self.layer,removeFromSuperlayer];let _:()=msg_send![self.layer,release];}}
}
#[derive(Default)]
struct PaintRegionAtlas {
    layers:Vec<PaintSurfaceLayer>,
    slots:Vec<PaintAtlasSurface>,
    size:(u64,u64),
    displayed:Option<usize>,
}
impl MetalRenderer {
    fn paint_atlas_enabled()->bool {
        static ATLAS:std::sync::OnceLock<bool>=std::sync::OnceLock::new();
        *ATLAS.get_or_init(||std::env::var("KYBERN_REGION_LAYERS").as_deref()==Ok("1")&&std::env::var("KYBERN_REGION_ATLAS").as_deref()==Ok("1"))
    }
    fn new_region_surface(width:u64,height:u64)->Result<PaintAtlasSurface>{
        unsafe {
            let properties:*mut objc::runtime::Object=msg_send![objc::class!(NSMutableDictionary),dictionary];
            for (key,value) in [(kIOSurfaceWidth,width),(kIOSurfaceHeight,height),(kIOSurfaceBytesPerElement,4),(kIOSurfacePixelFormat,0x42475241)] {
                let number:*mut objc::runtime::Object=msg_send![objc::class!(NSNumber),numberWithUnsignedLongLong:value];
                let _:()=msg_send![properties,setObject:number forKey:key];
            }
            let surface=IOSurfaceCreate(properties as *const _);
            if surface.is_null(){anyhow::bail!("IOSurface allocation failed ({width}x{height})");}
            Ok(PaintAtlasSurface(surface))
        }
    }
    fn region_surface_texture(&self,surface:&PaintAtlasSurface,size:(u64,u64))->Result<metal::Texture>{
        let desc=metal::TextureDescriptor::new();
        desc.set_width(size.0);desc.set_height(size.1);desc.set_pixel_format(MTLPixelFormat::BGRA8Unorm);
        desc.set_storage_mode(metal::MTLStorageMode::Shared);
        desc.set_usage(metal::MTLTextureUsage::RenderTarget|metal::MTLTextureUsage::ShaderRead);
        unsafe {
            let texture:*mut objc::runtime::Object=msg_send![&*self.device,newTextureWithDescriptor:&*desc iosurface:surface.0 plane:0u64];
            if texture.is_null(){anyhow::bail!("IOSurface Metal texture allocation failed");}
            Ok(metal::Texture::from_ptr(texture as *mut _))
        }
    }
    fn render_packed_texture(&mut self,scene:&Scene,viewport:Size<DevicePixels>,texture:&metal::TextureRef,rectangles:&[metal::MTLScissorRect],offsets:&[(i64,i64)],existing:Option<&metal::CommandBufferRef>)->Result<metal::CommandBuffer>{
        let mut writer=InstanceBufferWriter::new(&self.device,&self.instance_buffer_pool,self.is_unified_memory);
        let bindings=write_instances(scene,&mut writer)?;
        let command=existing.map(|c|c.to_owned()).unwrap_or_else(||self.command_queue.new_command_buffer().to_owned());
        let damage=self.paint_region_scissor;
        self.paint_region_origin=offsets.first().copied();
        self.paint_region_crops=rectangles.to_vec();self.paint_region_packed=offsets.to_vec();
        self.paint_region_command=Some(command.clone());
        let result=self.draw_primitives_to_texture(scene,&bindings,&mut writer,texture,viewport);
        self.paint_region_origin=None;self.paint_region_scissor=damage;
        self.paint_region_crops.clear();self.paint_region_packed.clear();self.paint_region_command=None;
        let pool=self.instance_buffer_pool.clone();let buffer=Cell::new(Some(writer.finish()));
        let block=ConcreteBlock::new(move |_|{if let Some(buffer)=buffer.take(){pool.lock().release(buffer);}}).copy();
        command.add_completed_handler(&block);
        if let Err(error)=result {command.commit();return Err(error);}
        Ok(command)
    }
    fn present_packed_regions(&mut self,scene:&Scene,viewport:Size<DevicePixels>)->bool {
        match self.try_present_packed_regions(scene,viewport) {
            Ok(value)=>value,
            Err(error)=>{log::error!("[paint-region] atlas-error {error:#}");false},
        }
    }
    fn try_present_packed_regions(&mut self,scene:&Scene,viewport:Size<DevicePixels>)->Result<bool>{
        if !self.is_unified_memory||!self.is_apple_gpu{return Ok(false);}
        if !self.prepare_passive_region(scene,viewport,false)?{return Ok(false);}
        let Some(bounds)=scene.passive_paint_bounds else{return Ok(false);};
        let width=i32::from(viewport.width).max(1) as u64;let height=i32::from(viewport.height).max(1) as u64;
        let x=bounds.origin.x.0.floor().max(0.).min(width as f32) as u64;
        let y=bounds.origin.y.0.floor().max(0.).min(height as f32) as u64;
        let right=(bounds.origin.x.0+bounds.size.width.0).ceil().max(x as f32).min(width as f32) as u64;
        let bottom=(bounds.origin.y.0+bounds.size.height.0).ceil().max(y as f32).min(height as f32) as u64;
        let damage=self.paint_region_scissor;
        self.paint_region_scissor=Some(metal::MTLScissorRect{x,y,width:right-x,height:bottom-y});
        let mut regions:Vec<_>=self.paint_region_rectangles(scene,viewport).into_iter().flatten().collect();
        self.paint_region_scissor=damage;
        regions.sort_by_key(|r|(r.y,r.x,r.width,r.height));
        if regions.is_empty(){return Ok(false);}
        let equal=|a:&metal::MTLScissorRect,b:&metal::MTLScissorRect|a.x==b.x&&a.y==b.y&&a.width==b.width&&a.height==b.height;
        let resources=self.paint_region_resources.as_mut().unwrap();
        let reconstruct=resources.generation!=scene.paint_generation||resources.regions.len()!=regions.len()||!resources.regions.iter().zip(&regions).all(|(a,b)|equal(a,b));
        let mut atlas=resources.atlas.take().unwrap_or_default();
        // A two-pixel gutter keeps filtering within a tile at fractional scales.
        let mut positions=Vec::new();let mut cx=0;let mut cy=0;let mut row=0;let mut used_width=0;
        let limit=regions.iter().map(|r|r.width+4).max().unwrap().max(4096);
        for r in &regions {
            if cx+r.width+4>limit {cy+=row;cx=0;row=0;}
            positions.push((cx+2,cy+2));cx+=r.width+4;row=row.max(r.height+4);used_width=used_width.max(cx);
        }
        let needed=(((used_width+31)/32)*32,((cy+row+31)/32)*32);
        if needed.0>16384||needed.1>16384{self.paint_region_resources.as_mut().unwrap().atlas=Some(atlas);return Ok(false);}
        if atlas.size.0<needed.0||atlas.size.1<needed.1 {
            atlas.slots.clear();atlas.displayed=None;atlas.size=needed;
        }
        let free=atlas.slots.iter().enumerate().find(|(index,s)|Some(*index)!=atlas.displayed&&unsafe{IOSurfaceIsInUse(s.0)==0}).map(|(index,_)|index);
        let slot=if let Some(index)=free {index}else if atlas.slots.len()<8 {
            atlas.slots.push(Self::new_region_surface(atlas.size.0,atlas.size.1)?);
            if std::env::var_os("KYBERN_DEBUG_REGION").is_some(){eprintln!("[paint-region] atlas {}x{} slots={}",atlas.size.0,atlas.size.1,atlas.slots.len());}
            atlas.slots.len()-1
        }else{
            if std::env::var_os("KYBERN_DEBUG_REGION").is_some(){eprintln!("[paint-region] atlas-backpressure");}
            self.paint_region_resources.as_mut().unwrap().atlas=Some(atlas);return Ok(true);
        };
        let texture=self.region_surface_texture(&atlas.slots[slot],atlas.size)?;
        let Some(parent)=self.layer.clone() else{return Ok(false);};
        let base=if reconstruct||atlas.layers.is_empty(){parent.next_drawable().map(|d|d.to_owned())}else{None};
        if (reconstruct||atlas.layers.is_empty())&&base.is_none(){self.paint_region_resources.as_mut().unwrap().atlas=Some(atlas);return Ok(false);}
        let base_command=if let Some(drawable)=base.as_ref(){
            self.paint_region_scissor=None;self.paint_region_base_holes=regions.clone();
            let result=self.render_frame(scene,drawable.texture(),viewport);
            self.paint_region_base_holes.clear();self.paint_region_scissor=damage;
            Some(result?)
        }else{None};
        let mut expanded=Vec::new();let mut offsets=Vec::new();
        for (r,(tx,ty)) in regions.iter().zip(&positions){
            let x=r.x.saturating_sub(2);let y=r.y.saturating_sub(2);
            expanded.push(metal::MTLScissorRect{x,y,width:(r.x+r.width+2).min(width)-x,height:(r.y+r.height+2).min(height)-y});
            offsets.push((r.x as i64-*tx as i64,r.y as i64-*ty as i64));
        }
        let command=self.render_packed_texture(scene,viewport,&texture,&expanded,&offsets,base_command.as_deref())?;
        self.paint_region_scissor=Some(metal::MTLScissorRect{x,y,width:right-x,height:bottom-y});
        if std::env::var("KYBERN_GPU_TRACE").as_deref()==Ok("1"){eprintln!("[paint-region] atlas-work target={}x{} damage={} regions={} base={}",atlas.size.0,atlas.size.1,regions.iter().map(|r|r.width*r.height).sum::<u64>(),regions.len(),base.is_some());}
        self.trace_paint_region(&command,scene,viewport,true,"atlas");
        self.paint_region_scissor=damage;
        command.commit();command.wait_until_completed();
        if command.status()!=metal::MTLCommandBufferStatus::Completed{anyhow::bail!("packed region GPU command failed");}
        // GPU writes are complete before the IOSurface is handed to CA. Reuse
        // only surfaces that IOSurfaceIsInUse says the compositor has released.
        let bounds:cocoa::foundation::NSRect=unsafe{msg_send![&*parent,bounds]};
        if bounds.size.width<=0. || bounds.size.height<=0. {self.paint_region_resources.as_mut().unwrap().atlas=Some(atlas);return Ok(false);}
        let sx=width as f64/bounds.size.width;let sy=height as f64/bounds.size.height;
        let flipped:bool=unsafe{msg_send![&*parent,isGeometryFlipped]};
        let same=atlas.layers.len()==regions.len()&&atlas.layers.iter().zip(&regions).all(|(a,b)|equal(&a.rect,b));
        if !same {
            atlas.layers.clear();
            for r in &regions {
                let layer:*mut objc::runtime::Object=unsafe{msg_send![objc::class!(CALayer),new]};
                let frame=cocoa::foundation::NSRect::new(cocoa::foundation::NSPoint::new(bounds.origin.x+r.x as f64/sx,bounds.origin.y+if flipped{r.y as f64/sy}else{bounds.size.height-(r.y+r.height) as f64/sy}),NSSize::new(r.width as f64/sx,r.height as f64/sy));
                unsafe{let _:()=msg_send![layer,setFrame:frame];let _:()=msg_send![layer,setContentsScale:sx];let _:()=msg_send![layer,setOpaque:self.opaque];let _:()=msg_send![&*parent,addSublayer:layer];}
                atlas.layers.push(PaintSurfaceLayer{layer,rect:*r});
            }
            if std::env::var_os("KYBERN_DEBUG_REGION").is_some(){eprintln!("[paint-region] layers {} atlas",atlas.layers.len());}
        }
        for ((layer,r),(tx,ty)) in atlas.layers.iter().zip(&regions).zip(&positions){
            let source=cocoa::foundation::NSRect::new(cocoa::foundation::NSPoint::new(*tx as f64/atlas.size.0 as f64,1.-(*ty+r.height) as f64/atlas.size.1 as f64),NSSize::new(r.width as f64/atlas.size.0 as f64,r.height as f64/atlas.size.1 as f64));
            unsafe{let _:()=msg_send![layer.layer,setContentsRect:source];let _:()=msg_send![layer.layer,setContents:atlas.slots[slot].0];}
        }
        if let Some(drawable)=base.as_ref(){drawable.present();}
        atlas.displayed=Some(slot);
        let resources=self.paint_region_resources.as_mut().unwrap();
        resources.generation=scene.paint_generation;resources.regions=regions;resources.atlas=Some(atlas);
        self.paint_region_scissor=None;
        Ok(true)
    }
}
