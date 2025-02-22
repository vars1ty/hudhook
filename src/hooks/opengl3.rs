use std::ffi::CString;
use std::sync::OnceLock;
use std::time::Instant;

use imgui::Context;
use parking_lot::Mutex;
use tracing::{debug, trace};
use windows::core::PCSTR;
use windows::Win32::Foundation::{
    GetLastError, HANDLE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{ScreenToClient, WindowFromDC, HDC};
use windows::Win32::Graphics::OpenGL::{glClearColor, glGetIntegerv, GL_VIEWPORT};
use windows::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
#[cfg(target_arch = "x86")]
use windows::Win32::UI::WindowsAndMessaging::SetWindowLongA;
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
use windows::Win32::UI::WindowsAndMessaging::SetWindowLongPtrA;
use windows::Win32::UI::WindowsAndMessaging::{
    DefWindowProcW, GetClientRect, GetCursorPos, GetForegroundWindow, IsChild, GWLP_WNDPROC,
};

use crate::hooks::common::{imgui_wnd_proc_impl, ImguiWindowsEventHandler, WndProcType};
use crate::hooks::{Hooks, ImguiRenderLoop};
use crate::mh::MhHook;
use crate::renderers::imgui_opengl3::get_proc_address;

type OpenGl32wglSwapBuffers = unsafe extern "system" fn(HDC) -> ();

unsafe fn draw(dc: HDC) {
    // Get the imgui renderer, or create it if it does not exist
    let mut imgui_renderer = IMGUI_RENDERER
        .get_or_insert_with(|| {
            // Create ImGui context
            let mut context = imgui::Context::create();
            context.set_ini_filename(None);

            // Initialize the render loop with the context
            IMGUI_RENDER_LOOP.get_mut().unwrap().initialize(&mut context);

            let renderer = imgui_opengl::Renderer::new(&mut context, |s| {
                get_proc_address(CString::new(s).unwrap()) as _
            });

            // Grab the HWND from the DC
            let hwnd = WindowFromDC(dc);

            // Set the new wnd proc, and assign the old one to a variable for further
            // storing
            #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
            let wnd_proc = std::mem::transmute::<_, WndProcType>(SetWindowLongPtrA(
                hwnd,
                GWLP_WNDPROC,
                imgui_wnd_proc as usize as isize,
            ));
            #[cfg(target_arch = "x86")]
            let wnd_proc = std::mem::transmute::<_, WndProcType>(SetWindowLongA(
                hwnd,
                GWLP_WNDPROC,
                imgui_wnd_proc as usize as i32,
            ));

            // Create the imgui rendererer
            let mut imgui_renderer = ImguiRenderer {
                ctx: context,
                renderer,
                wnd_proc,
                game_hwnd: hwnd,
                resolution_and_rect: None,
            };

            // Initialize window events on the imgui renderer
            ImguiWindowsEventHandler::setup_io(&mut imgui_renderer);

            // Return the imgui renderer as a mutex
            Mutex::new(Box::new(imgui_renderer))
        })
        .lock();

    imgui_renderer.render();
}

unsafe extern "system" fn imgui_wnd_proc(
    hwnd: HWND,
    umsg: u32,
    WPARAM(wparam): WPARAM,
    LPARAM(lparam): LPARAM,
) -> LRESULT {
    if IMGUI_RENDERER.is_some() {
        match IMGUI_RENDERER.as_mut().unwrap().try_lock() {
            Some(imgui_renderer) => imgui_wnd_proc_impl(
                hwnd,
                umsg,
                WPARAM(wparam),
                LPARAM(lparam),
                imgui_renderer,
                IMGUI_RENDER_LOOP.get().unwrap(),
            ),
            None => {
                debug!("Could not lock in WndProc");
                DefWindowProcW(hwnd, umsg, WPARAM(wparam), LPARAM(lparam))
            },
        }
    } else {
        debug!("WndProc called before hook was set");
        DefWindowProcW(hwnd, umsg, WPARAM(wparam), LPARAM(lparam))
    }
}

#[allow(non_snake_case)]
unsafe extern "system" fn imgui_opengl32_wglSwapBuffers_impl(dc: HDC) {
    trace!("opengl32.wglSwapBuffers invoked");

    // Draw ImGui
    draw(dc);

    // If resolution or window rect changes - reset ImGui
    reset(dc);

    // Get the trampoline
    let trampoline_wglswapbuffers =
        TRAMPOLINE.get().expect("opengl32.wglSwapBuffers trampoline uninitialized");

    // Call the original function
    trampoline_wglswapbuffers(dc)
}

unsafe fn reset(hdc: HDC) {
    if IMGUI_RENDERER.is_none() {
        return;
    }

    if let Some(mut renderer) = IMGUI_RENDERER.as_mut().unwrap().try_lock() {
        // Get resolution
        let viewport = &mut [0; 4];
        glGetIntegerv(GL_VIEWPORT, viewport.as_mut_ptr());

        let hwnd = WindowFromDC(hdc);
        let rect = get_client_rect(&hwnd).unwrap();

        let (resolution, window_rect) =
            renderer.resolution_and_rect.get_or_insert(([viewport[2], viewport[3]], rect));

        // Compare previously saved to current
        if viewport[2] != resolution[0]
            || viewport[3] != resolution[1]
            || rect.right != window_rect.right
            || rect.bottom != window_rect.bottom
        {
            renderer.cleanup();
            glClearColor(0.0, 0.0, 0.0, 1.0);
            IMGUI_RENDERER.take();
        }
    }
}

static mut IMGUI_RENDER_LOOP: OnceLock<Box<dyn ImguiRenderLoop + Send + Sync>> = OnceLock::new();
static mut IMGUI_RENDERER: Option<Mutex<Box<ImguiRenderer>>> = None;
static TRAMPOLINE: OnceLock<OpenGl32wglSwapBuffers> = OnceLock::new();

struct ImguiRenderer {
    ctx: Context,
    renderer: imgui_opengl::Renderer,
    wnd_proc: WndProcType,
    game_hwnd: HWND,
    resolution_and_rect: Option<([i32; 2], RECT)>,
}

fn get_client_rect(hwnd: &HWND) -> Option<RECT> {
    unsafe {
        let mut rect: RECT = core::mem::zeroed();
        if GetClientRect(*hwnd, &mut rect).is_ok() {
            Some(rect)
        } else {
            None
        }
    }
}

static mut LAST_FRAME: Option<Mutex<Instant>> = None;

impl ImguiRenderer {
    unsafe fn render(&mut self) {
        if let Some(rect) = get_client_rect(&self.game_hwnd) {
            let io = self.ctx.io_mut();
            io.display_size = [(rect.right - rect.left) as f32, (rect.bottom - rect.top) as f32];
            let mut pos = POINT { x: 0, y: 0 };

            let active_window = GetForegroundWindow();
            if !HANDLE(active_window.0).is_invalid()
                && (active_window == self.game_hwnd
                    || IsChild(active_window, self.game_hwnd).as_bool())
            {
                let gcp = GetCursorPos(&mut pos as *mut _);
                if gcp.is_ok() && ScreenToClient(self.game_hwnd, &mut pos as *mut _).as_bool() {
                    io.mouse_pos[0] = pos.x as _;
                    io.mouse_pos[1] = pos.y as _;
                }
            }
        } else {
            trace!("GetClientRect error: {:?}", GetLastError());
        }

        // Update the delta time of ImGui as to tell it how long has elapsed since the
        // last frame
        let last_frame = LAST_FRAME.get_or_insert_with(|| Mutex::new(Instant::now())).get_mut();
        let now = Instant::now();
        self.ctx.io_mut().update_delta_time(now.duration_since(*last_frame));
        *last_frame = now;

        let ui = self.ctx.frame();

        IMGUI_RENDER_LOOP.get_mut().unwrap().render(ui);
        self.renderer.render(&mut self.ctx);
    }

    unsafe fn cleanup(&mut self) {
        #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
        SetWindowLongPtrA(self.game_hwnd, GWLP_WNDPROC, self.wnd_proc as usize as isize);

        #[cfg(target_arch = "x86")]
        SetWindowLongA(self.game_hwnd, GWLP_WNDPROC, self.wnd_proc as usize as i32);
    }
}

impl ImguiWindowsEventHandler for ImguiRenderer {
    fn io(&self) -> &imgui::Io {
        self.ctx.io()
    }

    fn io_mut(&mut self) -> &mut imgui::Io {
        self.ctx.io_mut()
    }

    fn wnd_proc(&self) -> WndProcType {
        self.wnd_proc
    }
}
unsafe impl Send for ImguiRenderer {}
unsafe impl Sync for ImguiRenderer {}

// Get the address of wglSwapBuffers in opengl32.dll
unsafe fn get_opengl_wglswapbuffers_addr() -> OpenGl32wglSwapBuffers {
    // Grab a handle to opengl32.dll
    let opengl32dll = CString::new("opengl32.dll").unwrap();
    let opengl32module = GetModuleHandleA(PCSTR(opengl32dll.as_ptr() as *mut _))
        .expect("failed finding opengl32.dll");

    // Grab the address of wglSwapBuffers
    let wglswapbuffers = CString::new("wglSwapBuffers").unwrap();
    let wglswapbuffers_func =
        GetProcAddress(opengl32module, PCSTR(wglswapbuffers.as_ptr() as *mut _)).unwrap();

    std::mem::transmute(wglswapbuffers_func)
}

/// Stores hook detours and implements the [`Hooks`] trait.
pub struct ImguiOpenGl3Hooks([MhHook; 1]);

impl ImguiOpenGl3Hooks {
    /// # Safety
    ///
    /// Is most likely undefined behavior, as it modifies function pointers at
    /// runtime.
    pub unsafe fn new<T: 'static>(t: T) -> Self
    where
        T: ImguiRenderLoop + Send + Sync,
    {
        // Grab the addresses
        let hook_opengl_swapbuffers_address = get_opengl_wglswapbuffers_addr();

        // Create detours
        let hook_opengl_wgl_swap_buffers = MhHook::new(
            hook_opengl_swapbuffers_address as *mut _,
            imgui_opengl32_wglSwapBuffers_impl as *mut _,
        )
        .expect("couldn't create opengl32.wglSwapBuffers hook");

        // Initialize the render loop and store detours
        IMGUI_RENDER_LOOP.get_or_init(|| Box::new(t));
        TRAMPOLINE.get_or_init(|| std::mem::transmute(hook_opengl_wgl_swap_buffers.trampoline()));

        Self([hook_opengl_wgl_swap_buffers])
    }
}

impl Hooks for ImguiOpenGl3Hooks {
    fn from_render_loop<T>(t: T) -> Box<Self>
    where
        Self: Sized,
        T: ImguiRenderLoop + Send + Sync + 'static,
    {
        Box::new(unsafe { ImguiOpenGl3Hooks::new(t) })
    }

    fn hooks(&self) -> &[MhHook] {
        &self.0
    }

    unsafe fn unhook(&mut self) {
        if let Some(renderer) = IMGUI_RENDERER.take() {
            renderer.lock().cleanup();
        }
        drop(IMGUI_RENDER_LOOP.take());
    }
}
