use anyhow::Result;
use gst::{
    prelude::Cast,
    traits::{ElementExt, GstObjectExt},
};
use gstreamer as gst;
use gstreamer_gl::{
    prelude::ContextGLExt,
    traits::{GLContextExt, GLFramebufferExt},
};
use gstreamer_video::VideoInfo;
use smithay::{
    backend::{
        egl::{native::EGLSurfacelessDisplay, EGLContext, EGLDisplay},
        renderer::{gles2::Gles2Renderer, Frame, Renderer},
    },
    utils::{Rectangle, Size, Transform},
};

fn main() -> Result<()> {
    gst::init()?;

    let egl = EGLDisplay::new(EGLSurfacelessDisplay, None)?;
    let context = EGLContext::new(&egl, None)?;

    let gst_gl_egl_display = unsafe {
        gstreamer_gl_egl::GLDisplayEGL::with_egl_display(**egl.get_display_handle() as usize)
    }?;

    let gst_gl_context = unsafe {
        gstreamer_gl::GLContext::new_wrapped(
            &gst_gl_egl_display,
            context.get_context_handle() as usize,
            gstreamer_gl::GLPlatform::EGL,
            gstreamer_gl::GLAPI::GLES2,
        )
    }
    .ok_or_else(|| anyhow::anyhow!("no gl context"))?;

    let mut renderer =
        unsafe { Gles2Renderer::new(context, None) }.expect("Failed to initialize renderer");

    gst_gl_context.activate(true)?;
    renderer
        .with_context(|_| {
            gst_gl_context
                .fill_info()
                .expect("failed to fill gl context info");
        })
        .expect("Failed with_context");

    let allocator = gstreamer_gl::GLMemoryAllocator::default(&gst_gl_context);

    let caps = gst::Caps::builder("video/x-raw")
        .features([gstreamer_gl::CAPS_FEATURE_MEMORY_GL_MEMORY])
        .field("format", "BGRA")
        .field("width", 1920i32)
        .field("height", 1080i32)
        .field("texture-target", "2D")
        .build();
    let video_info = VideoInfo::from_caps(&caps).unwrap();

    // Example on how to allocator gl memory
    let allocation_params = gstreamer_gl::GLVideoAllocationParams::new(
        &gst_gl_context,
        None,
        &video_info,
        0,
        None,
        gstreamer_gl::GLTextureTarget::_2d,
        gstreamer_gl::GLFormat::Rgba,
    );

    let allocation_params = unsafe { std::mem::transmute(allocation_params) };

    let mut gl_memory = gstreamer_gl::GLBaseMemoryRef::alloc(&allocator, &allocation_params)
        .expect("failed to alloc gl memory");
    dbg!(&gl_memory);

    // Example for gl framebuffer
    let framebuffer = gstreamer_gl::GLFramebuffer::new(&gst_gl_context);
    dbg!(&framebuffer);

    unsafe {
        framebuffer.attach(
            smithay::backend::renderer::gles2::ffi::COLOR_ATTACHMENT0,
            &mut gl_memory,
        );
    }

    let mut map_info = std::mem::MaybeUninit::zeroed();
    unsafe {
        gst::ffi::gst_memory_map(
            gl_memory.as_mut_ptr() as *mut _,
            map_info.as_mut_ptr(),
            gst::ffi::GST_MAP_WRITE | gstreamer_gl::ffi::GST_MAP_GL as u32,
        );
    }

    framebuffer.bind();
    if !gst_gl_context.check_framebuffer_status(smithay::backend::renderer::gles2::ffi::FRAMEBUFFER)
    {
        panic!("Framebuffer status error");
    }

    let mut frame = renderer.render(Size::from((1920, 1080)), Transform::Normal)?;
    frame.clear(
        [1f32, 0f32, 0f32, 1f32],
        &[Rectangle::from_loc_and_size((0, 0), (1920, 1080))],
    )?;
    frame.finish()?;

    gst_gl_context.clear_framebuffer();

    unsafe {
        gst::ffi::gst_memory_unmap(gl_memory.as_mut_ptr() as *mut _, map_info.as_mut_ptr());
    }

    let map = gl_memory.map_readable().expect("failed to map readable");

    let image_buffer: image::ImageBuffer<image::Rgba<u8>, _> =
        image::ImageBuffer::from_raw(1920, 1080, map.as_slice()).unwrap();
    image_buffer.save("/tmp/test.jpeg").unwrap();

    return Ok(());

    // Example on how to provide the context to elements in the pipeline
    let pipeline = gst::Pipeline::new(None);

    let bus = pipeline
        .bus()
        .ok_or_else(|| anyhow::anyhow!("pipeline without bus?"))?;

    bus.set_sync_handler(move |_bus, message| {
        match message.view() {
            gst::MessageView::NeedContext(ctxt) => {
                let context_type = ctxt.context_type();
                eprintln!("need context: {}", context_type);
                if context_type == *gstreamer_gl::GL_DISPLAY_CONTEXT_TYPE {
                    if let Some(el) = message
                        .src()
                        .map(|s| s.downcast_ref::<gst::Element>().unwrap())
                    {
                        eprintln!("setting gl display on element: {}", el.name());
                        let context = gst::Context::new(context_type, true);
                        context.set_gl_display(&gst_gl_egl_display);
                        el.set_context(&context);
                    }
                }
                if context_type == "gst.gl.app_context" {
                    if let Some(el) = message
                        .src()
                        .map(|s| s.downcast_ref::<gst::Element>().unwrap())
                    {
                        eprintln!("setting gl context on element: {}", el.name());
                        let mut context = gst::Context::new(context_type, true);
                        {
                            let context = context.get_mut().unwrap();
                            let s = context.structure_mut();
                            s.set("context", &gst_gl_context);
                        }
                        el.set_context(&context);
                    }
                }
            }
            gst::MessageView::StreamStatus(status) => {
                let t = status
                    .structure()
                    .unwrap()
                    .get::<gst::StreamStatusType>("type")
                    .expect("wrong type");
                let owner = status
                    .structure()
                    .unwrap()
                    .get::<gst::Element>("owner")
                    .expect("wrong type");

                if t == gst::StreamStatusType::Enter && owner.name() == "appsrc" {
                    eprintln!("activating context in thread");
                    gst_gl_context
                        .activate(true)
                        .expect("failed to activate context");
                }

                if t == gst::StreamStatusType::Leave && owner.name() == "appsrc" {
                    eprintln!("de-activating context in thread");
                    gst_gl_context
                        .activate(false)
                        .expect("failed to active context");
                }
            }
            _ => (),
        }
        gst::BusSyncReply::Pass
    });

    pipeline.set_state(gst::State::Ready)?;

    for msg in bus.iter_timed(gst::ClockTime::NONE) {
        use gst::MessageView;

        match msg.view() {
            MessageView::Eos(..) => break,
            MessageView::Error(err) => {
                anyhow::bail!("error in pipeline: {:?}", err);
            }
            _ => (),
        }
    }

    pipeline.set_state(gst::State::Null)?;

    Ok(())
}
